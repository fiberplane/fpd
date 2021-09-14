use clap::Clap;
use futures::{sink::SinkExt, stream::StreamExt};
use hyper::{Body, Request, Response, Server, StatusCode};
use hyper_tungstenite::tungstenite;
use rmp_serde::Serializer;
use routerify::ext::RequestExt;
use routerify::{Router, RouterService};
use serde::Serialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tungstenite::Message;
use uuid::Uuid;

use crate::common::{FetchDataMessage, FetchDataResultMessage, RelayMessage, ServerMessage};

#[derive(Clap)]
pub struct Arguments {}

pub async fn handle_command(_: Arguments) {
    let relay_service = RelayService {
        connections: Default::default(),
        outgoing_queries: Default::default(),
    };

    let service = RouterService::new(router(relay_service)).unwrap();

    // We'll bind to 127.0.0.1:3000
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));

    let server = Server::bind(&addr)
        .serve(service)
        .with_graceful_shutdown(shutdown_signal());

    eprintln!("Starting server {}", addr);

    match server.await {
        Ok(_) => eprintln!("Graceful shutdown"),
        Err(e) => eprintln!("server error: {}", e),
    }
}

async fn shutdown_signal() {
    // Wait for the CTRL+C signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
    eprintln!("Received shutdown signal");
}

#[derive(Debug, Clone)]
pub struct RelayService {
    connections: Arc<Mutex<HashMap<Uuid, tokio::sync::mpsc::UnboundedSender<ServerMessage>>>>,
    outgoing_queries:
        Arc<Mutex<HashMap<Uuid, tokio::sync::oneshot::Sender<FetchDataResultMessage>>>>,
}

impl RelayService {
    fn handle_message(&self, message: RelayMessage) {
        match message {
            RelayMessage::FetchDataResult(message) => {
                self.handle_fetch_data_result_message(message)
            }
        }
    }

    fn handle_fetch_data_result_message(&self, message: FetchDataResultMessage) {
        let reply = {
            let mut outgoing_queries = self.outgoing_queries.lock().unwrap();
            outgoing_queries.remove(&message.op_id)
        };

        match reply {
            Some(chan) => {
                eprintln!(
                    "handle_fetch_data_result_message: received something for a outgoing message"
                );
                if !chan.is_closed() {
                    eprintln!("handle_fetch_data_result_message: channel is not closed");
                    chan.send(message)
                        .expect("unable to send message to outgoing queries");
                }
            }
            None => {
                eprintln!("handle_fetch_data_result_message: received message for a unknown outgoing query");
            }
        };
    }

    /// Register a channel which allows messages to be send back to the
    /// proxy server.
    fn register_connection(
        &self,
        id: uuid::Uuid,
        tx: tokio::sync::mpsc::UnboundedSender<ServerMessage>,
    ) {
        let mut connections = self.connections.lock().unwrap();
        connections.insert(id, tx);
    }

    /// Register a channel that will be called once a result is received with
    /// the same op_id.
    pub fn prepare_query(
        &self,
        op_id: Uuid,
    ) -> tokio::sync::oneshot::Receiver<FetchDataResultMessage> {
        let (rx, tx) = tokio::sync::oneshot::channel();

        let mut outgoing_queries = self.outgoing_queries.lock().unwrap();
        if let Some(_) = outgoing_queries.insert(op_id, rx) {
            eprintln!("outgoing query was replaced");
        };

        tx
    }
}

async fn ws_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    eprintln!("ws_handler");

    // Check if the request is a websocket upgrade request.
    if hyper_tungstenite::is_upgrade_request(&req) {
        eprintln!("ws_handler: tis a upgrade request");

        // at this point we should be able to inspect the token that is in in
        // the req, authorize it, and update registration.

        let id = uuid::Uuid::nil();
        let service = req.service().clone();

        let (mut response, websocket) =
            hyper_tungstenite::upgrade(req, None).expect("upgrade should succeed");

        response
            .headers_mut()
            .insert("x-fp-conn-id", id.to_string().parse().unwrap());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
        service.register_connection(id, tx);

        // Spawn a task to handle the websocket connection.
        tokio::spawn(async move {
            eprintln!("ws_handler: making a websocket stream");
            let websocket = websocket.await.expect("unable to get websocket");

            eprintln!("ws_handler: splitting websocket stream");

            let (mut write, mut read) = websocket.split();
            eprintln!("ws_handler: making a websocket stream is finished");

            eprintln!("ws_handler: creating read and write loop");

            // read loop
            let read_handle = tokio::spawn(async move {
                eprintln!("ws_handler: starting read loop");
                loop {
                    let message = read.next().await;
                    match message.unwrap().unwrap() {
                        Message::Text(_) => todo!(),
                        Message::Binary(message) => {
                            eprintln!("ws_handler: received binary message");
                            let message = RelayMessage::deserialize_msgpack(message);
                            service.handle_message(message);
                        }
                        Message::Ping(_) => todo!(),
                        Message::Pong(_) => todo!(),
                        Message::Close(_) => break,
                    }
                }
                read
            });

            // write loop
            let write_handle = tokio::spawn(async move {
                eprintln!("ws_handler: starting write loop");
                while let Some(message) = rx.recv().await {
                    eprintln!("ws_handler: sending message to server");

                    let mut buf = Vec::new();
                    message.serialize(&mut Serializer::new(&mut buf)).unwrap();

                    write
                        .send(Message::Binary(buf))
                        .await
                        .expect("should be good");
                }
                write
            });

            eprintln!("ws_handler: waiting for read and write to stop");
            let (read, write) = futures::join!(read_handle, write_handle);

            eprintln!("ws_handler: reuniting read and write, and closing them");
            let websocket = read.unwrap().reunite(write.unwrap());
            websocket.unwrap().close(None).await.ok();
        });

        eprintln!("ws_handler: returning id: {}", &id.to_string());

        // Return the response so the spawned future can continue.
        Ok(response)
    } else {
        // Handle regular HTTP requests here.
        Ok(Response::new(Body::from("Hello HTTP!")))
    }
}

async fn relay_handler(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    eprintln!("relay_handler");

    // TODO: Read query from body and forward to proxy server
    // TODO: Get proxy id and DS from request

    let service = req.service();

    let connection_exists = {
        let connections = service.connections.lock().unwrap();
        connections.get(&uuid::Uuid::nil()).is_some()
    };

    // NOTE: it is possible for a connection to have bee removed between these
    // checks. This will be refactored in a future PR.
    if connection_exists {
        eprintln!("relay_handler: sending message");
        let op_id = uuid::Uuid::new_v4();
        let fetch_data_message = FetchDataMessage {
            op_id,
            query: "".to_owned(),
        };
        let message = ServerMessage::FetchData(fetch_data_message);

        let mut buf = Vec::new();
        message.serialize(&mut Serializer::new(&mut buf)).unwrap();

        let rx = service.prepare_query(op_id);

        {
            let connections = service.connections.lock().unwrap();
            let connection = connections.get(&uuid::Uuid::nil()).unwrap();
            connection.send(message).ok();
        }

        let result = rx.await.unwrap();

        return Ok(Response::new(Body::from(result.result.unwrap())));
    } else {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("NOT FOUND"))
            .unwrap());
    }
}

async fn not_found_handler(_: Request<Body>) -> Result<Response<Body>, Infallible> {
    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("NOT FOUND"))
        .unwrap())
}

// Create a `Router<Body, Infallible>` for response body type `hyper::Body`
// and for handler error type `Infallible`.
fn router(service: RelayService) -> Router<Body, Infallible> {
    let state = State::new(service);

    Router::builder()
        // Specify the state data which will be available to every route handlers,
        // error handler and middlewares.
        .data(state)
        .get("/ws", ws_handler)
        .get("/relay", relay_handler)
        .any(not_found_handler)
        .build()
        .unwrap()
}

pub trait ApiRequestExt {
    fn state(&self) -> &State;

    fn service(&self) -> &RelayService {
        self.state().service()
    }
}

/// This impl covers both hyper::Request and hyper::Parts.
impl<T: RequestExt> ApiRequestExt for T {
    fn state(&self) -> &State {
        &self.data::<State>().expect("State must be set")
    }
}

// Global state.
#[derive(Clone)]
pub struct State {
    service: RelayService,
}

impl State {
    pub fn new(service: RelayService) -> Self {
        State { service }
    }

    pub fn service(&self) -> &RelayService {
        &self.service
    }
}
