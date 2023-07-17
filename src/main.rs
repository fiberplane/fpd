pub mod cli;
pub mod runtime;
pub mod tasks;

use anyhow::{anyhow, bail};
use clap::Parser;
use std::{io, path::PathBuf, process, str::FromStr};
use tasks::service::{ProxyDataSource, ProxyService};
use tokio::fs;
use tracing::{error, info, trace, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), anyhow::Error> {
    let args = cli::Arguments::parse();

    initialize_logger(&args);

    if let Some(subcommand) = args.subcommand {
        match subcommand {
            cli::Action::Config { action } => match action {
                cli::ConfigAction::Paths { query } => {
                    if query.is_none() {
                        println!(
                            "Expected location for 'data_sources.yaml': {:?}",
                            runtime::data_sources_path()?
                        );
                        println!(
                            "Expected directory for providers: {:?}",
                            runtime::providers_wasm_dir()?
                        );
                        return Ok(());
                    }

                    match query.unwrap() {
                        cli::ConfigPathQuery::DataSources => {
                            print!("{}", runtime::data_sources_path()?.display())
                        }
                        cli::ConfigPathQuery::WasmDir => {
                            print!("{}", runtime::providers_wasm_dir()?.display())
                        }
                    }
                    return Ok(());
                }
            },
            cli::Action::Pull(args) => {
                tasks::provider_manager::pull(args).await?;
                return Ok(());
            }
            cli::Action::BuildProviders(args) => {
                tasks::provider_manager::build_providers(args)?;
                return Ok(());
            }
        }
    }

    let wasm_dir = {
        if args.wasm_dir.is_some() {
            args.wasm_dir.clone().unwrap()
        } else if PathBuf::from_str("./providers")
            .map_or(false, |path| path.exists() && path.is_dir())
        {
            PathBuf::from_str("./providers").unwrap()
        } else {
            runtime::providers_wasm_dir()?
        }
    };

    if !wasm_dir.is_dir() {
        bail!("wasm_dir ({wasm_dir:?}) must be a directory");
    }

    let data_sources_path = {
        if args.data_sources_path.is_some() {
            args.data_sources_path.clone().unwrap()
        } else if PathBuf::from_str("./data_sources.yaml")
            .map_or(false, |path| path.exists() && path.is_file())
        {
            PathBuf::from_str("./data_sources.yaml").unwrap()
        } else {
            runtime::data_sources_path()?
        }
    };

    // Load data sources config file
    let data_sources = {
        match fs::read_to_string(&data_sources_path).await {
            Ok(data_sources) => data_sources,
            Err(err) => {
                match err.kind() {
                    io::ErrorKind::NotFound => {
                        bail!(
                            "Data sources file not found at {} ({})",
                            data_sources_path.display(),
                            err
                        );
                    }
                    io::ErrorKind::PermissionDenied => {
                        bail!(
                            "Insufficient permissions to read data sources file {} ({})",
                            data_sources_path.display(),
                            err
                        );
                    }
                    _ => {
                        bail!(
                            "Unable to read data sources file at {}: {}",
                            data_sources_path.display(),
                            err
                        );
                    }
                };
            }
        }
    };

    let data_sources: Vec<ProxyDataSource> =
        serde_yaml::from_str(&data_sources).expect("Invalid 'data_sources.yaml' file");

    let proxy = ProxyService::init(
        args.api_base,
        args.token.ok_or_else(|| {
            anyhow!(
                "TOKEN is mandatory to run Fiberplane Daemon. See {} --help",
                clap::crate_name!()
            )
        })?,
        wasm_dir.as_path(),
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
            info!("Daemon shutdown successfully");
            Ok(())
        }
        Err(err) => {
            error!(?err, "daemon encountered an error");
            bail!("Daemon encountered an error: {err:?}");
        }
    }
}

fn initialize_logger(args: &cli::Arguments) {
    let env_filter = if let Some(rust_log) = &args.rust_log {
        EnvFilter::from_str(rust_log).expect("Invalid RUST_LOG value")
    } else if let Some(log_level) = args.log_level {
        // Enable logs from both the daemon and the provider runtime
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
