//! Command Line Interface types and Argument parsing

use anyhow::{anyhow, Error};
use clap::{Parser, Subcommand, ValueEnum};
use fiberplane::models::proxies::ProxyToken;
use std::{net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};
use tracing::Level;
use url::Url;

use crate::tasks::provider_manager::{BuildProvidersArgs, PullProvidersArgs};

#[derive(Parser)]
#[clap(author, about, version, verbatim_doc_comment)]
/// The Fiberplane Daemon connects Fiberplane Studio to your data sources.
///
/// It enables secure communication between Fiberplane and your data
/// using WebAssembly-based providers and token-based authentication.
/// Set up a token either:
/// - using `fp` command line tool with `fp daemon create --help`, or
/// - online in Studio web user interface.
pub struct Arguments {
    /// Path to directory containing provider WASM files
    #[clap(long, env)]
    pub wasm_dir: Option<PathBuf>,

    /// Web-socket endpoint of the Fiberplane API (leave path empty to use the default path)
    #[clap(long, short, env, default_value = "wss://studio.fiberplane.com", aliases = &["FIBERPLANE_ENDPOINT", "fiberplane-endpoint"])]
    pub api_base: Url,

    /// Token used to authenticate against the Fiberplane API. This is created through the CLI by running the command: `fp daemon add`
    #[clap(long, short, env)]
    pub token: Option<ProxyToken>,

    /// Path to data sources YAML file
    #[clap(long, short, env)]
    pub data_sources_path: Option<PathBuf>,

    /// Max retries to connect to the fiberplane server before giving up on failed connections
    #[clap(long, short, env, default_value = "10")]
    pub max_retries: u32,

    /// Address to bind HTTP server to (used for health check endpoints)
    #[clap(long, short, env)]
    pub listen_address: Option<SocketAddr>,

    /// Interval to check the status of each data source ("30s" = 30 seconds, "5m" = 5 minutes, "1h" = 1 hour)
    #[clap(long, short, env, default_value = "5m")]
    pub status_check_interval: IntervalDuration,

    /// Set the logging level for the daemon (trace, debug, info, warn, error)
    #[clap(long, env)]
    pub log_level: Option<Level>,

    #[clap(env, hide = true)]
    pub rust_log: Option<String>,

    /// Output logs as JSON
    #[clap(long, env)]
    pub log_json: bool,

    #[clap(subcommand)]
    pub subcommand: Option<Action>,
}

#[derive(Subcommand)]
pub enum Action {
    /// Configuration discovery when no environement variable or development setup is detected
    Config {
        #[clap(subcommand)]
        action: ConfigAction,
    },

    /// Pull Fiberplane providers from GitHub
    Pull(PullProvidersArgs),

    /// Rebuild Fiberplane providers from a local `providers` repository checkout
    BuildProviders(BuildProvidersArgs),
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print the canonical configuration paths, used when no environment variable
    /// or local development setup is detected.
    ///
    /// Call without argument to print all paths
    Paths {
        /// Configuration item to ask for
        #[arg(value_enum)]
        query: Option<ConfigPathQuery>,
    },
}
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum ConfigPathQuery {
    /// Path to the data_sources.yaml file to use to configure the daemon
    DataSources,
    /// Path to the directory containing the WebAssembly providers
    WasmDir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalDuration(pub Duration);

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
