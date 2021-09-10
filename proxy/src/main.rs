use clap::{AppSettings, Clap};

mod common;
mod relay;
mod server;

#[derive(Clap)]
#[clap(author, about, version, setting = AppSettings::ColoredHelp)]
struct Arguments {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Clap)]
enum SubCommand {
    #[clap(name = "server", about = "Start the proxy server")]
    Server(server::Arguments),

    #[clap(name = "relay", about = "Start the proxy relay component")]
    Relay(relay::Arguments),
}

#[tokio::main]
async fn main() {
    let args = Arguments::parse();

    use SubCommand::*;
    match args.subcmd {
        Server(args) => server::handle_command(args).await,
        Relay(args) => relay::handle_command(args).await,
    }
}
