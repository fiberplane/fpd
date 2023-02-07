use super::websocket_keepalive::{WebSocketKeepAlive, DEFAULT_PING_TIMEOUT};
use async_channel::{bounded, Receiver, SendError, Sender};
use futures::{select_biased, FutureExt};
use http::Request;
use std::sync::Arc;
use std::{cmp, time::Duration};
use tokio::spawn;
use tokio::sync::{watch, Mutex};
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, error::ProtocolError, Error, Message,
};
use tracing::{debug, error, trace};

#[cfg(test)]
mod tests;

const DEFAULT_MAX_RETRIES: u32 = 10;
const DEFAULT_MAX_BACKOFF_DURATION: Duration = Duration::from_secs(60);

/// Connect to the given WebSocket server
pub async fn connect_async(
    request: impl IntoClientRequest,
) -> Result<ReconnectingWebSocket, Error> {
    let websocket = ReconnectingWebSocket::new(request)?;
    websocket.connect().await?;
    Ok(websocket)
}

pub type HandshakeResponse = tokio_tungstenite::tungstenite::handshake::client::Response;
pub type ResponseHandler = Box<dyn Fn(HandshakeResponse) + Send + Sync>;

pub struct Builder {
    request: Request<()>,
    max_retries: u32,
    max_backoff_duration: Duration,
    ping_timeout: Duration,
    connect_response_handler: Option<ResponseHandler>,
}

impl Builder {
    fn new(request: impl IntoClientRequest) -> Result<Self, Error> {
        Ok(Self {
            request: request.into_client_request()?,
            max_retries: DEFAULT_MAX_RETRIES,
            max_backoff_duration: DEFAULT_MAX_BACKOFF_DURATION,
            ping_timeout: DEFAULT_PING_TIMEOUT,
            connect_response_handler: None,
        })
    }

    /// Maximum number of times the WebSocket should try to reconnect
    /// if it encounters an error. Defaults to 10.
    ///
    /// Note that the total number of connection attempts is one more
    /// than the configured number of retries.
    pub fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Maximum amount of time the WebSocket should wait before attempting
    /// to reconnect. Defaults to 60 seconds.
    pub fn max_backoff_duration(mut self, duration: Duration) -> Self {
        self.max_backoff_duration = duration;
        self
    }

    /// Automatically send pings after this amount of inactivity.
    /// Defaults to 45 seconds.
    pub fn ping_timeout(mut self, duration: Duration) -> Self {
        self.ping_timeout = duration;
        self
    }

    /// The given function will be called every time the websocket
    /// connects and can be used to extract the headers from the
    /// server's HTTP response.
    pub fn connect_response_handler(
        mut self,
        handler: impl Fn(HandshakeResponse) + Send + Sync + 'static,
    ) -> Self {
        self.connect_response_handler = Some(Box::new(handler));
        self
    }

    pub fn build(self) -> ReconnectingWebSocket {
        let (outgoing_sender, outgoing_receiver) = bounded(1);
        let (retry_sender, retry_receiver) = bounded(1);
        let (incoming_sender, incoming_receiver) = bounded(1);
        let (close_sender, close_receiver) = watch::channel(false);
        let (disconnect_sender, disconnect_receiver) = watch::channel(false);

        ReconnectingWebSocket(Arc::new(Inner {
            request: self.request,
            max_retries: self.max_retries,
            max_backoff_duration: self.max_backoff_duration,
            ping_timeout: self.ping_timeout,
            connect_response_handler: self.connect_response_handler.map(Arc::new),
            outgoing_sender,
            outgoing_receiver,
            retry_sender,
            retry_receiver,
            incoming_sender,
            incoming_receiver,
            close_sender: Mutex::new(close_sender),
            close_receiver,
            disconnect_sender: Mutex::new(disconnect_sender),
            disconnect_receiver,
        }))
    }
}

#[derive(Clone)]
pub struct ReconnectingWebSocket(Arc<Inner>);

struct Inner {
    request: Request<()>,
    max_retries: u32,
    max_backoff_duration: Duration,
    ping_timeout: Duration,
    connect_response_handler: Option<Arc<ResponseHandler>>,
    incoming_sender: Sender<Result<Message, Error>>,
    incoming_receiver: Receiver<Result<Message, Error>>,
    outgoing_sender: Sender<Message>,
    outgoing_receiver: Receiver<Message>,
    retry_sender: Sender<Message>,
    retry_receiver: Receiver<Message>,
    close_sender: Mutex<watch::Sender<bool>>,
    close_receiver: watch::Receiver<bool>,
    disconnect_sender: Mutex<watch::Sender<bool>>,
    disconnect_receiver: watch::Receiver<bool>,
}

impl ReconnectingWebSocket {
    pub fn new(request: impl IntoClientRequest) -> Result<ReconnectingWebSocket, Error> {
        Builder::new(request).map(|builder| builder.build())
    }

    pub fn builder(request: impl IntoClientRequest) -> Result<Builder, Error> {
        Builder::new(request)
    }

    /// Connect to the server, retrying up to the configured number of attempts
    pub async fn connect(&self) -> Result<(), Error> {
        let mut attempt = 0;
        loop {
            trace!(?attempt, "connect");
            match self.initiate_connection().await {
                Ok(()) => {
                    return Ok(());
                }
                Err(err) => {
                    if attempt == self.0.max_retries {
                        return Err(err);
                    }

                    // Return final errors immediately
                    if let Error::Http(response) = &err {
                        if response.status().is_client_error() {
                            error!(?err, "error connecting to server");
                            return Err(err);
                        }
                    }
                }
            }

            let duration = Duration::from_millis(2u64.pow(attempt + 6));
            let duration = cmp::min(duration, self.0.max_backoff_duration);
            trace!(?duration, "waiting before reconnecting");
            sleep(duration).await;

            attempt += 1;
        }
    }

    /// Send a Message via the WebSocket
    pub async fn send(&self, message: Message) -> Result<(), Error> {
        trace!(?message, "send");
        if *self.0.close_receiver.borrow() {
            return Err(Error::AlreadyClosed);
        }
        self.0
            .outgoing_sender
            .send(message)
            .await
            .map_err(|_| Error::ConnectionClosed)
    }

    /// Handle the next incoming WebSocket message.
    /// Returns None if the WebSocket closed normally.
    pub async fn recv(&self) -> Option<Result<Message, Error>> {
        let mut close_receiver = self.0.close_receiver.clone();
        loop {
            select_biased! {
                result = self.0.incoming_receiver.recv().fuse() => match result {
                    Ok(Err(Error::Protocol(ProtocolError::ResetWithoutClosingHandshake))) => continue,
                    Err(_) => return Some(Err(Error::ConnectionClosed)),
                    Ok(result) => return Some(result),
                },
                result = close_receiver.changed().fuse() => {
                    if result.is_err() || *close_receiver.borrow() {
                        return None;
                    }
                }
            }
        }
    }

    /// Gracefully close the WebSocket connection
    /// and send a Close message to the server.
    /// Note: this will close all clones of this WebSocket as well.
    pub async fn close(&self) {
        trace!("close");
        self.0.close_sender.lock().await.send_replace(true);
    }

    /// Returns true if the underlying websocket is currently connected
    pub fn is_connected(&self) -> bool {
        !*self.0.close_receiver.borrow() && !*self.0.disconnect_receiver.borrow()
    }

    /// Connect to the server and, if successful, spawn tasks to forward
    /// messages between the websocket and the incoming/outgoing channels
    async fn initiate_connection(&self) -> Result<(), Error> {
        trace!("connecting to: {}", self.0.request.uri());

        let request = clone_request(&self.0.request);
        let config = WebSocketConfig {
            // tungstenite-rs has a minimum send queue size of 1
            max_send_queue: Some(1),
            ..WebSocketConfig::default()
        };
        let (ws, response) =
            match tokio_tungstenite::connect_async_with_config(request, Some(config)).await {
                Ok(result) => result,
                Err(err) => {
                    debug!(?err, "Error connecting to websocket server");
                    return Err(err);
                }
            };
        trace!("websocket connection established");

        let ws = WebSocketKeepAlive::new_with_ping_timeout(ws, self.0.ping_timeout);

        // Spawn two tasks to handle incoming and outgoing messages
        self.spawn_read_loop(ws.clone());
        self.spawn_write_loop(ws);

        if let Some(handler) = &self.0.connect_response_handler {
            handler(response);
        }

        Ok(())
    }

    /// Spawn a task to forward messages from the underlying WebSocketKeepAlive's
    /// incoming channel to our incoming channel.
    ///
    /// This will stop if the WebSocket closes, receives an error, or if the status changes
    /// to Disconnected or Closed.
    fn spawn_read_loop(&self, ws: WebSocketKeepAlive) {
        let clone = Arc::downgrade(&self.0);
        let mut close_receiver = self.0.close_receiver.clone();
        let incoming_sender = self.0.incoming_sender.clone();
        let mut disconnect_receiver = self.0.disconnect_receiver.clone();

        spawn(async move {
            trace!("read loop started");
            if let Some(clone) = clone.upgrade() {
                clone.disconnect_sender.lock().await.send_replace(false);
            }
            #[allow(unused_assignments)]
            let should_reconnect = loop {
                select_biased! {
                    message = ws.recv().fuse() => {
                        match message {
                            None => {
                                trace!("incoming websocket channel closed");
                                break true;
                            },
                            Some(Ok(Message::Close(frame))) => {
                                // TODO decide whether to reconnect based on the frame
                                trace!(?frame, "received close frame");
                                break false;
                            },
                            Some(Err(err)) => {
                                error!(?err, "received websocket error");
                                if let Err(err) = incoming_sender.send(Err(err)).await {
                                    error!(?err, "error forwarding incoming error to channel");
                                    break false;
                                }
                            },
                            Some(Ok(message)) => {
                                if let Err(err) = incoming_sender.send(Ok(message)).await {
                                    error!(?err, "error forwarding incoming message to channel");
                                    break false;
                                }
                            }
                        }
                    },
                    result = disconnect_receiver.changed().fuse() => {
                        if result.is_err() || *disconnect_receiver.borrow() {
                            trace!("write loop disconnected, stopping read loop");
                            break true;
                        }
                    },
                    result = close_receiver.changed().fuse() => {
                        if result.is_err() || *close_receiver.borrow() {
                            trace!("closed, not forwarding any more incoming messages to the channel");
                            break false;
                        }
                    }
                }
            };
            trace!("read loop finished");

            if let Some(clone) = clone.upgrade() {
                // Tell the write loop to stop
                clone.disconnect_sender.lock().await.send_replace(true);

                if should_reconnect {
                    trace!("reconnecting");
                    ReconnectingWebSocket(clone).connect().await.ok();
                } else {
                    trace!("not reconnecting");
                }
            }
        });
    }

    /// Spawn a task to forward all messages from the outgoing channel
    /// to the WebSocketKeepAlive's outgoing channel.
    ///
    /// This will stop if all senders on the incoming channel are dropped
    /// or if the status changes to Disconnected or Close.
    fn spawn_write_loop(&self, ws: WebSocketKeepAlive) {
        let clone = Arc::downgrade(&self.0);
        let mut close_receiver = self.0.close_receiver.clone();
        let mut disconnect_receiver = self.0.disconnect_receiver.clone();
        let outgoing_receiver = self.0.outgoing_receiver.clone();
        let retry_sender = self.0.retry_sender.clone();
        let retry_receiver = self.0.retry_receiver.clone();
        spawn(async move {
            trace!("write loop started");
            loop {
                select_biased! {
                    message = retry_receiver.recv().fuse() => {
                        if let Ok(message) = message {
                            trace!(?message, "retrying forwarding message to websocket");

                            if let Err(SendError(message)) = ws.outgoing_sender.send(message).await {
                                debug!("error retrying forwarding outgoing message to websocket. will resend on reconnect");
                                if let Err(err) = retry_sender.send(message).await {
                                    error!(?err, "error queuing message to be resent");
                                }

                                break;
                            }
                        }
                    }
                    message = outgoing_receiver.recv().fuse() => {
                        if let Ok(message) = message {
                            trace!(?message, "forwarding message to websocket");

                            if let Err(SendError(message)) = ws.outgoing_sender.send(message).await {
                                debug!("error forwarding outgoing message to websocket. will resend on reconnect");
                                if let Err(err) = retry_sender.send(message).await {
                                    error!(?err, "error queuing message to be resent");
                                }

                                break;
                            }
                        } else {
                            trace!("outgoing channel closed, not forwarding any more messages to the websocket");
                            break;
                        }
                    },
                    result = disconnect_receiver.changed().fuse() => {
                        if result.is_err() || *disconnect_receiver.borrow() {
                            trace!("read loop disconnected, stopping write loop");
                            break;
                        }
                    }
                    result = close_receiver.changed().fuse() => {
                        if result.is_err() || *close_receiver.borrow() {
                            trace!("closed, not forwarding any more outgoing messages to the websocket");
                            break;
                        }
                    }
                }
            }
            trace!("write loop finished");

            // Tell the read loop to stop
            if let Some(clone) = clone.upgrade() {
                clone.disconnect_sender.lock().await.send_replace(true);
            }
        });
    }
}

// Manually clone the Request (because it does not implement Clone)
fn clone_request(original: &Request<()>) -> Request<()> {
    let mut request = Request::builder()
        .method(original.method())
        .uri(original.uri());
    let headers = request.headers_mut().unwrap();
    for (name, value) in original.headers() {
        headers.insert(name, value.clone());
    }
    request.body(()).expect("cloning request")
}

#[test]
fn clone_request_test() {
    let original = Request::builder()
        .method("POST")
        .uri("http://example.com")
        .header("my-header", "hello")
        .body(())
        .unwrap();
    let request = clone_request(&original);
    assert_eq!(request.uri(), "http://example.com");
    assert_eq!(request.method(), "POST");
    assert_eq!(
        request
            .headers()
            .get(http::header::HeaderName::from_static("my-header")),
        Some(&http::HeaderValue::from_static("hello"))
    );
}
