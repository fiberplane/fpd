use crate::service::ProxyService;
use clap::{AppSettings, Clap};
use data_sources::DataSources;
use std::path::PathBuf;
use tokio::fs;
use url::Url;

pub mod common;
mod data_sources;
mod service;

#[derive(Clap)]
#[clap(author, about, version, setting = AppSettings::ColoredHelp)]
pub struct Arguments {
    #[clap(
        env = "FP_PROXY_WASM_DIR",
        about = "Path to directory containing provider WASM files"
    )]
    wasm_dir: PathBuf,

    #[clap(
        long,
        short,
        env = "FP_PROXY_ENDPOINT",
        default_value = "ws://127.0.0.1:3000/ws"
    )]
    endpoint: Url,

    #[clap(long, short, env = "FP_PROXY_AUTH_TOKEN")]
    auth_token: String,

    #[clap(
        long,
        short,
        env = "FP_PROXY_DATA_SOURCES",
        about = "Path to data sources YAML file"
    )]
    data_sources: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let args = Arguments::parse();

    if !args.wasm_dir.is_dir() {
        panic!("wasm_dir must be a directory");
    }

    let data_sources = fs::read_to_string(args.data_sources)
        .await
        .expect("error reading data sources YAML file");
    let data_sources: DataSources =
        serde_yaml::from_str(&data_sources).expect("invalid data sources file");

    let proxy = ProxyService::new(args.endpoint, args.auth_token, args.wasm_dir, data_sources);
    proxy.connect().await.unwrap();
}
