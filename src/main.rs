use crate::service::{ProxyDataSource, ProxyService};
use anyhow::{anyhow, Error};
use clap::Parser;
use fiberplane::protocols::proxies::ProxyToken;
use std::{io, net::SocketAddr, path::PathBuf, process, str::FromStr, time::Duration};
use tokio::fs;
use tracing::{error, info, trace, warn, Level};
use tracing_subscriber::EnvFilter;
use url::Url;

mod metrics;
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
    #[clap(long, short, env, default_value = "wss://studio.fiberplane.com", aliases = &["FIBERPLANE_ENDPOINT", "fiberplane-endpoint"])]
    api_base: Url,

    /// Token used to authenticate against the Fiberplane API. This is created through the CLI by running the command: `fp proxy add`
    #[clap(long, short, env)]
    token: ProxyToken,

    /// Path to data sources YAML file
    #[clap(long, short, env, default_value = "data_sources.yaml")]
    data_sources_path: PathBuf,

    /// Max retries to connect to the fiberplane server before giving up on failed connections
    #[clap(long, short, env, default_value = "10")]
    max_retries: u32,

    /// Address to bind HTTP server to (used for health check endpoints)
    #[clap(long, short, env)]
    listen_address: Option<SocketAddr>,

    /// Interval to check the status of each data source ("30s" = 30 seconds, "5m" = 5 minutes, "1h" = 1 hour)
    #[clap(long, short, env, default_value = "5m")]
    status_check_interval: IntervalDuration,

    /// Set the logging level for the proxy (trace, debug, info, warn, error)
    #[clap(long, env)]
    log_level: Option<Level>,

    #[clap(env, hide = true)]
    rust_log: Option<String>,

    /// Output logs as JSON
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
    let args = Arguments::parse();

    initialize_logger(&args);

    if !args.wasm_dir.is_dir() {
        panic!("wasm_dir must be a directory");
    }

    // Load data sources config file
    let data_sources = {
        match fs::read_to_string(&args.data_sources_path).await {
            Ok(data_sources) => data_sources,
            Err(err) => {
                match err.kind() {
                    io::ErrorKind::NotFound => {
                        error!(
                            "Data sources file not found at {} ({})",
                            args.data_sources_path.display(),
                            err
                        );
                    }
                    io::ErrorKind::PermissionDenied => {
                        error!(
                            "Insufficient permissions to read data sources file {} ({})",
                            args.data_sources_path.display(),
                            err
                        );
                    }
                    _ => {
                        error!(
                            "Unable to read data sources file at {}: {}",
                            args.data_sources_path.display(),
                            err
                        );
                    }
                };
                process::exit(1);
            }
        }
    };
    let data_sources: Vec<ProxyDataSource> =
        serde_yaml::from_str(&data_sources).expect("Invalid data sources YAML file");

    let proxy = ProxyService::init(
        args.api_base,
        args.token,
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

fn initialize_logger(args: &Arguments) {
    let env_filter = if let Some(rust_log) = &args.rust_log {
        EnvFilter::from_str(&rust_log).expect("Invalid RUST_LOG value")
    } else if let Some(log_level) = args.log_level {
        // Enable logs from both the proxy and the provider runtime
        EnvFilter::new(format!(
            "{}={log_level},fp_provider_runtime={log_level}",
            env!("CARGO_PKG_NAME"),
            log_level = log_level
        ))
    } else {
        EnvFilter::from_default_env()
    };

    // Initialize the builder with some defaults
    let logger = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(io::stderr);

    if args.log_json {
        // Add a JSON formatter
        logger
            .json()
            .try_init()
            .expect("unable to initialize logging");
    } else {
        logger.try_init().expect("unable to initialize logging");
    }

    if args.rust_log.is_some() && args.log_level.is_some() {
        warn!("Both RUST_LOG and LOG_LEVEL are set, RUST_LOG will be used and LOG_LEVEL will be ignored");
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
