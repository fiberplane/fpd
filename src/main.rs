use crate::service::ProxyService;
use clap::{AppSettings, Clap};
use url::Url;

pub mod common;
mod service;

#[derive(Clap)]
#[clap(author, about, version, setting = AppSettings::ColoredHelp)]
pub struct Arguments {
    #[clap()]
    wasm_path: String,

    #[clap(
        long,
        short,
        env = "FP_PROXY_ENDPOINT",
        default_value = "ws://127.0.0.1:3000/ws"
    )]
    endpoint: String,

    #[clap(long, short, env = "FP_PROXY_AUTH_TOKEN")]
    auth_token: String,
}

#[tokio::main]
async fn main() {
    let args = Arguments::parse();
    let endpoint = Url::parse(&args.endpoint).expect("endpoint must be a valid URL");

    let proxy = ProxyService::new(endpoint, args.auth_token, args.wasm_path);
    proxy.connect().await;
}
