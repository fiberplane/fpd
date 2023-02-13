use super::Error;
use async_channel::{bounded, Receiver, Sender};
use futures::{select_biased, FutureExt, Sink, SinkExt, Stream, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex};
use tokio::{spawn, time::sleep};
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{error, trace};

pub(crate) const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(45);

#[cfg(test)]
mod tests;

/// WebSocket that wraps tokio-tungstenite to keep
/// the connection alive by sending pings automatically.
/// It also sets up channels for sending and receiving
/// messages, which are easier to work with than Streams
/// (for example, it means that the WebSocket can be easily cloned).
#[derive(Clone)]
pub struct WebSocketKeepAlive {
    ping_timeout: Duration,
    incoming_receiver: Receiver<Result<Message, Error>>,
    // Note that the ReconnectingWebSocket directly sends messages
    // to this channel so that if the sending fails, it gets a SendError<Message>
    // and can queue that message to be resent through another WebSocket
    pub(crate) outgoing_sender: Sender<Message>,
    disconnect_sender: Arc<Mutex<watch::Sender<bool>>>,
    disconnect_receiver: watch::Receiver<bool>,
}

impl WebSocketKeepAlive {
    /// Spawn tasks to forward messages between the WebSocket and the incoming/outgoing channels.
    /// Uses the default ping timeout of 45 seconds to keep the WebSocket alive.
    pub fn new<S>(ws: S) -> Self
    where
        S: Stream<Item = Result<Message, Error>>
            + Sink<Message, Error = Error>
            + Unpin
            + Send
            + 'static,
    {
        Self::new_with_ping_timeout(ws, DEFAULT_PING_TIMEOUT)
    }

    /// Spawn tasks to forward messages between the WebSocket and the incoming/outgoing channels
    pub fn new_with_ping_timeout<S>(ws: S, ping_timeout: Duration) -> Self
    where
        S: Stream<Item = Result<Message, tungstenite::Error>>
            + Sink<Message, Error = tungstenite::Error>
            + Unpin
            + Send
            + 'static,
    {
        let (incoming_sender, incoming_receiver) = bounded(1);
        let (outgoing_sender, outgoing_receiver) = bounded(1);
        let (disconnect_sender, disconnect_receiver) = watch::channel(false);

        let keepalive = WebSocketKeepAlive {
            ping_timeout,
            incoming_receiver,
            outgoing_sender,
            disconnect_sender: Arc::new(Mutex::new(disconnect_sender)),
            disconnect_receiver,
        };
        let (write_ws, read_ws) = ws.split();
        keepalive.spawn_write_loop(write_ws, outgoing_receiver);
        keepalive.spawn_read_loop(read_ws, incoming_sender);

        keepalive
    }

    /// Send a Message via the WebSocket
    pub async fn send(&self, message: Message) -> Result<(), Error> {
        trace!(?message, "send");
        let mut disconnect_receiver = self.disconnect_receiver.clone();
        if *disconnect_receiver.borrow() {
            return Err(Error::ConnectionClosed);
        }
        select_biased! {
            _ = disconnect_receiver.changed().fuse() => Err(Error::ConnectionClosed),
            result = self.outgoing_sender.send(message).fuse() => result.map_err(|_| Error::ConnectionClosed),
        }
    }

    /// Handle the next incoming WebSocket message.
    /// Returns None if the WebSocket is closed.
    pub async fn recv(&self) -> Option<Result<Message, Error>> {
        let result = self.incoming_receiver.recv().await.ok();
        trace!(?result, "recv");
        result
    }

    /// Gracefully close the WebSocket connection
    /// and send a Close message to the other party.
    /// Note: this will close all clones of this WebSocket as well.
    pub async fn close(&self) {
        trace!("close");
        self.disconnect_sender.lock().await.send_replace(true);
    }

    /// Returns true if the WebSocket is connected
    pub fn is_connected(&self) -> bool {
        !*self.disconnect_receiver.borrow()
    }

    /// Spawn a task to forward outgoing messages from the outgoing
    /// channel to the websocket
    fn spawn_write_loop<S>(&self, mut write_ws: S, outgoing_receiver: Receiver<Message>)
    where
        S: Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    {
        let disconnect_sender = self.disconnect_sender.clone();
        let mut disconnect_receiver = self.disconnect_receiver.clone();
        let ping_timeout = self.ping_timeout;
        spawn(async move {
            trace!("write loop started");
            loop {
                select_biased! {
                    _ = disconnect_receiver.changed().fuse() => {
                        trace!("disconnected, not forwarding any more messages to the websocket");
                        break;
                    }
                    message = outgoing_receiver.recv().fuse() => match message {
                        Ok(message) => {
                            trace!(?message, "sending outgoing message");
                            if let Err(err) = write_ws.send(message).await {
                                error!(?err, "error sending message to websocket");
                                // TODO what should be done with this message?
                                break;
                            }
                        },
                        Err(_) => {
                            trace!("outgoing channel closed, not sending any more messages");
                            break;
                        }
                    },
                    _ = sleep(ping_timeout).fuse() => {
                        trace!("sending ping");
                        if let Err(err) = write_ws.send(Message::Ping(b"ping".to_vec())).await {
                            error!(?err, "error sending ping to websocket");
                            break;
                        }
                    }
                }
            }

            // Clean up
            disconnect_sender.lock().await.send_replace(true);
            if let Err(err) = write_ws.close().await {
                error!(?err, "error closing websocket");
            }
            trace!("write loop finished");
        });
    }

    /// Spawn a task to pass incoming messages from the websocket
    /// to the incoming channel
    fn spawn_read_loop<S>(&self, mut read_ws: S, incoming_sender: Sender<Result<Message, Error>>)
    where
        S: Stream<Item = Result<Message, Error>> + Unpin + Send + 'static,
    {
        let disconnect_sender = self.disconnect_sender.clone();
        let mut disconnect_receiver = self.disconnect_receiver.clone();
        spawn(async move {
            trace!("read loop started");
            loop {
                trace!("read loop");
                select_biased! {
                    message = read_ws.next().fuse() => {
                        match message {
                            Some(Ok(Message::Ping(_))) => trace!("received ping"),
                            Some(Ok(Message::Pong(_))) => trace!("received pong"),
                            // TODO should we catch close messages or forward them?
                            Some(Ok(message)) => {
                                trace!(?message, "received message");
                                if let Err(err) = incoming_sender.send(Ok(message)).await {
                                    error!(?err, "error forwarding incoming message to channel");
                                    break;
                                }
                                trace!("sent message");
                            }
                            Some(Err(err)) => {
                                error!(?err, "received websocket error");
                                if let Err(err) = incoming_sender.send(Err(err)).await {
                                    error!(?err, "error forwarding incoming error to channel");
                                    break;
                                }
                            },
                            None => {
                                trace!("websocket closed");
                                break;
                            }
                        }
                    },
                    _ = disconnect_receiver.changed().fuse() => {
                        trace!("disconnected, not reading any more messages from the websocket");
                        break;
                    }

                }
            }
            disconnect_sender.lock().await.send_replace(true);
            trace!("read loop finished");
        });
    }
}
