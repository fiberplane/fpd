use crate::common::{FetchDataMessage, FetchDataResultMessage, RelayMessage, ServerMessage};
use clap::{AppSettings, Clap};
use fp_plugins::runtime;
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
}

#[tokio::main]
async fn main() {
    let args = Arguments::parse();

    // open ws connection

    let addr = url::Url::parse("ws://127.0.0.1:3000/ws").expect("valid endpoint");
    let (ws_stream, resp) = connect_async(addr).await.expect("failed to connect");

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

    let wasm_module = std::fs::read(wasm_path).unwrap();

    let engine = Universal::new(Singlepass::default()).engine();
    let store = Store::new(&engine);

    let runtime = runtime::Runtime::new(store, wasm_module).unwrap();

    let result = runtime
        .invoke::<String, String, String>("".to_owned())
        .await;

    let fetch_data_result_message = FetchDataResultMessage {
        op_id: message.op_id,
        result,
    };

    reply
        .send(RelayMessage::FetchDataResult(fetch_data_result_message))
        .ok();
}