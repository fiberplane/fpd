use crate::service::ProxyService;
use clap::{AppSettings, Clap};
use data_sources::DataSources;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;
use tokio::fs;
use tracing::{error, info, trace};
use url::Url;

mod data_sources;
mod service;
#[cfg(test)]
mod tests;

#[derive(Clap)]
#[clap(author, about, version, setting = AppSettings::ColoredHelp)]
pub struct Arguments {
    #[clap(
        long,
        env = "WASM_DIR",
        default_value = "./providers",
        about = "Path to directory containing provider WASM files"
    )]
    wasm_dir: PathBuf,

    #[clap(
        long,
        short,
        env = "FIBERPLANE_ENDPOINT",
        default_value = "wss://fiberplane.com",
        about = "Web-socket endpoint of the Fiberplane API (leave path empty to use the default path)"
    )]
    fiberplane_endpoint: Url,

    #[clap(
        long,
        short,
        env = "AUTH_TOKEN",
        about = "Token used to authenticate against the Fiberplane API. This is created through the CLI by running the command: `fp proxy add`"
    )]
    auth_token: String,

    #[clap(
        long,
        short,
        env = "DATA_SOURCES",
        default_value = "data_sources.yaml",
        about = "Path to data sources YAML file"
    )]
    data_sources: PathBuf,

    #[clap(
        long,
        short,
        env = "MAX_RETRIES",
        default_value = "10",
        about = "Max retries to connect to the fiberplane server before giving up on failed connections"
    )]
    max_retries: u32,

    #[clap(
        long,
        short,
        env = "LISTEN_ADDRESS",
        default_value = "127.0.0.1:3002",
        about = "Address to bind HTTP server to (used for health check endpoints)"
    )]
    listen_address: SocketAddr,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let mut args = Arguments::parse();

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
        Some(args.listen_address),
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
            error!(?err, "proxy encounterd a error");
            process::exit(1);
        }
    };
}
