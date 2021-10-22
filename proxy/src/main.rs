use crate::service::ProxyService;
use clap::{AppSettings, Clap};
use data_sources::DataSources;
use std::path::PathBuf;
use tokio::fs;
use url::Url;

mod data_sources;
mod service;

#[derive(Clap)]
#[clap(author, about, version, setting = AppSettings::ColoredHelp)]
pub struct Arguments {
    #[clap(
        long,
        env = "WASM_DIR",
        about = "Path to directory containing provider WASM files"
    )]
    wasm_dir: PathBuf,

    #[clap(
        long,
        short,
        env = "FIBERPLANE_ENDPOINT",
        default_value = "ws://127.0.0.1:3001",
        about = "Web-socket endpoint of the Fiberplane API (leave path empty to use the default path)"
    )]
    fiberplane_endpoint: Url,

    #[clap(
        long,
        short,
        env = "AUTH_TOKEN",
        about = "Token used to authenticate against the Fiberplane API"
    )]
    auth_token: String,

    #[clap(
        long,
        short,
        env = "DATA_SOURCES",
        about = "Path to data sources YAML file"
    )]
    data_sources: PathBuf,
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

    let proxy = ProxyService::create(
        args.fiberplane_endpoint,
        args.auth_token,
        args.wasm_dir.as_path(),
        data_sources,
    )
    .await
    .unwrap();

    proxy.connect().await.unwrap();
}
