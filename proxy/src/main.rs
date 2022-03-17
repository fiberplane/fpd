use crate::service::{parse_data_sources_yaml, DataSources, ProxyService};
use anyhow::{anyhow, Error};
use clap::Parser;
use std::{io, net::SocketAddr, path::PathBuf, process, str::FromStr, time::Duration};
use tokio::fs;
use tracing::{error, info, trace};
use url::Url;

mod service;
#[cfg(test)]
mod tests;

#[derive(Parser)]
#[clap(author, about, version)]
pub struct Arguments {
    /// Path to directory containing provider WASM files
    #[clap(long, env, default_value = "./providers")]
    wasm_dir: PathBuf,

    /// Web-socket endpoint of the Fiberplane API (leave path empty to use the default path)
    #[clap(long, short, default_value = "wss://fiberplane.com")]
    fiberplane_endpoint: Url,

    /// Token used to authenticate against the Fiberplane API. This is created through the CLI by running the command: `fp proxy add`
    #[clap(long, short, env)]
    auth_token: String,

    /// Path to data sources YAML file
    #[clap(long, short, env, default_value = "data_sources.yaml")]
    data_sources: PathBuf,

    /// Max retries to connect to the fiberplane server before giving up on failed connections
    #[clap(long, short, env, default_value = "10")]
    max_retries: u32,

    /// Address to bind HTTP server to (used for health check endpoints)
    #[clap(long, short, env)]
    listen_address: Option<SocketAddr>,

    /// Interval to check the status of each data source ("30s" = 30 seconds, "5m" = 5 minutes, "1h" = 1 hour)
    #[clap(long, short, env, default_value = "5m")]
    status_check_interval: IntervalDuration,

    #[clap(long, env)]
    log_json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntervalDuration(Duration);

impl FromStr for IntervalDuration {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_at(s.len() - 1) {
            (s, "s") => Ok(IntervalDuration(Duration::from_secs(u64::from_str(s)?))),
            (s, "m") => Ok(IntervalDuration(Duration::from_secs(
                u64::from_str(s)? * 60,
            ))),
            (s, "h") => Ok(IntervalDuration(Duration::from_secs(
                u64::from_str(s)? * 60 * 60,
            ))),
            _ => Err(anyhow!("invalid interval")),
        }
    }
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
        parse_data_sources_yaml(&data_sources).expect("invalid data sources file");

    let proxy = ProxyService::init(
        args.fiberplane_endpoint,
        args.auth_token,
        args.wasm_dir.as_path(),
        data_sources,
        args.max_retries,
        args.listen_address,
        args.status_check_interval.0,
    )
    .await;

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

#[test]
fn interval_parsing() {
    assert_eq!(
        IntervalDuration(Duration::from_secs(30)),
        "30s".parse().unwrap()
    );
    assert_eq!(
        IntervalDuration(Duration::from_secs(60)),
        "1m".parse().unwrap()
    );
    assert_eq!(
        IntervalDuration(Duration::from_secs(3600)),
        "1h".parse().unwrap()
    );
    IntervalDuration::from_str("3d").expect_err("invalid interval");
}
