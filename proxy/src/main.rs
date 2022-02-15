use crate::service::{DataSources, ProxyService};
use clap::Parser;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;
use tokio::fs;
use tracing::{error, info, trace};
use url::Url;

mod service;
#[cfg(test)]
mod tests;

#[derive(Parser)]
#[clap(author, about, version)]
pub struct Arguments {
    #[clap(long, env = "WASM_DIR", default_value = "./providers")]
    //Path to directory containing provider WASM files
    wasm_dir: PathBuf,

    #[clap(
        long,
        short,
        env = "FIBERPLANE_ENDPOINT",
        default_value = "wss://fiberplane.com"
    )]
    //Web-socket endpoint of the Fiberplane API (leave path empty to use the default path)
    fiberplane_endpoint: Url,

    #[clap(long, short, env = "AUTH_TOKEN")]
    //Token used to authenticate against the Fiberplane API. This is created through the CLI by running the command: `fp proxy add`
    auth_token: String,

    #[clap(long, short, env = "DATA_SOURCES", default_value = "data_sources.yaml")]
    //Path to data sources YAML file
    data_sources: PathBuf,

    #[clap(long, short, env = "MAX_RETRIES", default_value = "10")]
    //Max retries to connect to the fiberplane server before giving up on failed connections
    max_retries: u32,

    #[clap(long, short, env = "LISTEN_ADDRESS")]
    //Address to bind HTTP server to (used for health check endpoints)
    listen_address: Option<SocketAddr>,

    #[clap(long, env = "LOG_JSON")]
    log_json: bool,
}

#[tokio::main]
async fn main() {
    let mut args = Arguments::parse();

    initialize_logger(args.log_json);

    // Update the endpoint to include the default path if nothing is set
    if args.fiberplane_endpoint.path() == "/" {
        args.fiberplane_endpoint.set_path("/api/proxies/ws");
    }

    if !args.wasm_dir.is_dir() {
        panic!("wasm_dir must be a directory");
    }

    let data_sources = fs::read_to_string(args.data_sources)
        .await
        .expect("error reading data sources YAML file");
    let data_sources: DataSources =
        serde_yaml::from_str(&data_sources).expect("invalid data sources file");

    let proxy = ProxyService::init(
        args.fiberplane_endpoint,
        args.auth_token,
        args.wasm_dir.as_path(),
        data_sources,
        args.max_retries,
        args.listen_address,
    )
    .await
    .expect("Error initializing proxy");

    let (shutdown, _) = tokio::sync::broadcast::channel(3);

    let cloned_shutdown = shutdown.clone();
    ctrlc::set_handler(move || {
        info!("received SIGINT, shutting down listeners");
        if cloned_shutdown.send(()).is_err() {
            trace!("no listeners found");
            process::exit(0);
        }
    })
    .expect("Error setting Ctrl-C handler");

    match proxy.connect(shutdown).await {
        Ok(_) => {
            info!("proxy shutdown successfully");
        }
        Err(err) => {
            error!(?err, "proxy encountered a error");
            process::exit(1);
        }
    };
}

fn initialize_logger(log_json: bool) {
    // Initialize the builder with some defaults
    let builder = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr);

    if log_json {
        // Add a JSON formatter
        builder
            .json()
            .try_init()
            .expect("unable to initialize logging");
    } else {
        builder.try_init().expect("unable to initialize logging");
    }
}
