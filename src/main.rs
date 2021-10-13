use crate::common::{
    FetchDataMessage, FetchDataResultMessage, QueryResult, QueryType, RelayMessage, ServerMessage,
};
use clap::{AppSettings, Clap};
use fp_provider_runtime::spec::types::{
    DataSource, PrometheusDataSource, QueryInstantOptions, QuerySeriesOptions,
};
use futures::{sink::SinkExt, StreamExt};
use hyper_tungstenite::tungstenite::Message;
use rmp_serde::Serializer;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::connect_async;
use wasmer::{Singlepass, Store, Universal};

pub mod common;

#[derive(Clap)]
#[clap(author, about, version, setting = AppSettings::ColoredHelp)]
pub struct Arguments {
    #[clap()]
    wasm_path: String,

    #[clap(
        long,
        short,
        env = "FP_PROXY_ENDPOINT",
        default_value = "ws://127.0.0.1:3001/ws"
    )]
    endpoint: String,

    #[clap(long, short, env = "FP_PROXY_AUTH_TOKEN")]
    auth_token: String,
}

#[tokio::main]
async fn main() {
    let args = Arguments::parse();

    // open ws connection
    let request = http::Request::builder()
        .uri(args.endpoint.clone())
        .header("fp-auth-token", args.auth_token.clone())
        .body(())
        .unwrap();

    let (ws_stream, resp) = connect_async(request).await.expect("failed to connect");

    let connection_id = resp.headers().get("x-fp-conn-id");
    match connection_id {
        Some(val) => eprintln!(
            "connection established, connection id: {}",
            val.to_str().unwrap()
        ),
        None => eprintln!("connection established, no connection id provided"),
    }

    let (mut write, mut read) = ws_stream.split();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelayMessage>();

    let local = tokio::task::LocalSet::new();

    let read_handle = local.run_until(async move {
        tokio::task::spawn_local(async move {
            use hyper_tungstenite::tungstenite::Message::*;
            while let Some(message) = read.next().await {
                eprintln!("handle_command: received a message");

                match message.unwrap() {
                    Text(_) => eprintln!("Received Text"),
                    Binary(msg) => {
                        handle_message(
                            args.wasm_path.clone(),
                            ServerMessage::deserialize_msgpack(msg),
                            tx.clone(),
                        )
                        .await
                    }
                    Ping(_) => eprintln!("Received Ping"),
                    Pong(_) => eprintln!("Received Pong"),
                    Close(_) => {
                        eprintln!("Received Close");
                        break;
                    }
                }
            }

            read
        })
        .await
    });

    let write_handle = tokio::spawn(async move {
        eprintln!("handle_command: creating write_handle");

        while let Some(message) = rx.recv().await {
            eprintln!("handle_command: sending message to relay");

            let mut buf = Vec::new();
            message.serialize(&mut Serializer::new(&mut buf)).unwrap();

            write
                .send(Message::Binary(buf))
                .await
                .expect("unable to send message to relay");

            eprintln!("handle_command: sending message to relay complete");
        }

        write
    });

    // keep connection open and handle incoming connections
    let (read, write) = futures::join!(read_handle, write_handle);

    eprintln!("handle_command: reuniting read and write, and closing them");
    let websocket = read.unwrap().reunite(write.unwrap());

    eprintln!("closing connection");
    websocket.unwrap().close(None).await.ok();

    eprintln!("connection closed");
}

async fn handle_message(
    wasm_path: String,
    message: ServerMessage,
    reply: UnboundedSender<RelayMessage>,
) {
    match message {
        ServerMessage::FetchData(message) => {
            handle_relay_query_message(wasm_path, message, reply).await
        }
    };
}

async fn handle_relay_query_message(
    wasm_path: String,
    message: FetchDataMessage,
    reply: UnboundedSender<RelayMessage>,
) {
    eprintln!("received a relay message: {:?}", message);

    // TODO: Preload and/or cache the result
    let wasm_module = std::fs::read(wasm_path).unwrap();

    let engine = Universal::new(Singlepass::default()).engine();
    let store = Store::new(&engine);

    let runtime =
        fp_provider_runtime::Runtime::new(store, wasm_module).expect("unable to create runtime");

    let query = message.query;
    let data_source = DataSource::Prometheus(PrometheusDataSource {
        // TODO: read the data-source actually from a local file
        url: "https://prometheus.dev.fiberplane.io".into(),
    });

    // Execute either a series or an instant query
    let query_result = match message.query_type {
        QueryType::Series(time_range) => {
            let options = QuerySeriesOptions {
                data_source,
                time_range,
            };
            let result = runtime.fetch_series(query, options).await;
            QueryResult::Series(result.unwrap())
        }
        QueryType::Instant(time) => {
            let options = QueryInstantOptions { data_source, time };
            let result = runtime.fetch_instant(query, options).await;
            QueryResult::Instant(result.unwrap())
        }
    };

    // TODO: Better handling of invocation errors. Do we send something back in
    // that case, and/or log to stderr?

    let fetch_data_result_message = FetchDataResultMessage {
        op_id: message.op_id,
        result: query_result,
    };

    reply
        .send(RelayMessage::FetchDataResult(fetch_data_result_message))
        .ok();
}
