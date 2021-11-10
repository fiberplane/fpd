use crate::data_sources::{DataSource, DataSources};
use anyhow::{anyhow, Context, Result};
use fp_provider_runtime::Runtime;
use futures::select;
use futures::sink::SinkExt;
use futures::stream::{SplitSink, SplitStream};
use futures::{FutureExt, StreamExt};
use hyper_tungstenite::tungstenite::Message;
use hyper_tungstenite::WebSocketStream;
use proxy_types::{
    ErrorMessage, InvokeProxyMessage, InvokeProxyResponseMessage, RelayMessage, ServerMessage,
    SetDataSourcesMessage,
};
use rmp_serde::Serializer;
use serde::Serialize;
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use stream_cancel::StreamExt as CancelStreamExt;
use tokio::fs;
use tokio::net::TcpStream;
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinError;
use tokio::time::{sleep, Duration};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream};
use tracing::{debug, error, info, trace, warn};
use url::Url;
use wasmer::{Singlepass, Store, Universal};

const WS_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_EXPONENTIAL_BACKOFF_DURATION: u64 = 10000;

/// This is a mapping from the provider type to the bytes of the wasm module
pub type WasmModuleMap = HashMap<String, Vec<u8>>;

#[derive(Clone, Debug)]
pub struct ProxyService {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    endpoint: Url,
    auth_token: String,
    data_sources: DataSources,
    wasm_modules: WasmModuleMap,
    max_retries: u32,
}

impl ProxyService {
    /// Load the provider wasm files from the given directory and create a new Proxy instance
    pub async fn init(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_dir: &Path,
        data_sources: DataSources,
        max_retries: u32,
    ) -> Result<Self> {
        let wasm_modules = load_wasm_modules(wasm_dir, &data_sources).await?;
        Ok(ProxyService::new(
            fiberplane_endpoint,
            auth_token,
            wasm_modules,
            data_sources,
            max_retries,
        ))
    }

    pub(crate) fn new(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_modules: WasmModuleMap,
        data_sources: DataSources,
        max_retries: u32,
    ) -> Self {
        ProxyService {
            inner: Arc::new(Inner {
                endpoint: fiberplane_endpoint,
                auth_token,
                wasm_modules,
                data_sources,
                max_retries,
            }),
        }
    }

    pub async fn connect(&self, shutdown: Sender<()>) -> Result<()> {
        let mut current_try = 0;
        loop {
            current_try += 1;
            let current_retries = current_try - 1;

            if current_retries > self.inner.max_retries {
                return Err(anyhow!("unable to connect, exceeded max tries"));
            } else if current_retries > 0 {
                let sleep_duration = {
                    let base: u64 = 2;
                    let duration = base.pow(current_try + 6);
                    let duration = cmp::min(duration, MAX_EXPONENTIAL_BACKOFF_DURATION);
                    Duration::from_millis(duration)
                };

                info!(?sleep_duration, "waiting before trying to reconnect");
                tokio::time::sleep(sleep_duration).await;
            }

            info!(?current_try, "connecting to fiberplane");

            let (ws_stream, conn_id) = match self.connect_websocket().await {
                Ok(result) => result,
                Err(err) => {
                    error!(?err, "unable to connect to web-socket server");
                    continue;
                }
            };

            info!(?conn_id, "connection established");

            let (mut write_ws, read_ws) = ws_stream.split();

            // Send the list of data sources to the relay
            let data_sources: SetDataSourcesMessage = self
                .inner
                .data_sources
                .iter()
                .map(|(name, data_source)| (name.clone(), data_source.into()))
                .collect();
            debug!("sending data sources to relay: {:?}", data_sources);
            let message = RelayMessage::SetDataSources(data_sources);
            let message = Message::Binary(message.serialize_msgpack());
            write_ws.send(message).await?;

            // At this point we are fairly confident that the server is
            // connected and working. So we will reset the current_try
            current_try = 0;

            let (tx_relay_messages, rx_relay_messages) =
                tokio::sync::mpsc::unbounded_channel::<RelayMessage>();

            // We use a local task set because the Wasmer runtime embedded in the ProxyService
            // cannot be moved across threads (which would be necessary to spawn a task that
            // includes the service)
            let local = tokio::task::LocalSet::new();
            let read_handle = local.run_until(Self::handle_read_loop(
                shutdown.clone(),
                conn_id.clone(),
                read_ws,
                tx_relay_messages,
                self.clone(),
            ));

            let write_handle = tokio::spawn(Self::handle_write_loop(
                shutdown.clone(),
                conn_id.clone(),
                rx_relay_messages,
                write_ws,
            ));

            // keep connection open and handle incoming connections
            let (read, write) = futures::join!(read_handle, write_handle);

            match (read, write) {
                (Ok(should_reconnect), Ok(_)) => {
                    if should_reconnect {
                        warn!(?conn_id, "reconnecting web-socket connection");
                    } else {
                        trace!(?conn_id, "shutdown web-socket connection successfully");
                        break;
                    }
                }
                (read, write) => {
                    error!(
                        ?read,
                        ?write,
                        ?conn_id,
                        "unexpected error occurred in the read or write loop"
                    );
                }
            };
        }

        Ok(())
    }

    /// Connects to a web-socket server and returns the connection id and the
    /// web-socket stream.
    async fn connect_websocket(
        &self,
    ) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, String)> {
        // Create a request object. If this fails there is no point in
        // retrying so just return the error object.
        let request = http::Request::builder()
            .uri(self.inner.endpoint.as_str())
            .header("fp-auth-token", self.inner.auth_token.clone())
            .body(())?;

        let (ws_stream, resp) = connect_async(request).await?;

        let conn_id = resp
            .headers()
            .get("x-fp-conn-id")
            .and_then(|id| id.to_str().map(|hv| hv.to_owned()).ok())
            .ok_or_else(|| anyhow!("no connection id was returned"))?;

        Ok((ws_stream, conn_id))
    }

    /// Handle any incoming web socket messages (`read_ws`) by sending them
    /// to the service for processing.
    ///
    /// This will block until a message is broadcast on the `shutdown` channel.
    /// It can also exit if an error occurred during receiving or sending a
    /// message from the channel.
    async fn handle_read_loop(
        shutdown: Sender<()>,
        conn_id: String,
        read_ws: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        tx_relay_messages: UnboundedSender<RelayMessage>,
        service: ProxyService,
    ) -> Result<bool, JoinError> {
        use hyper_tungstenite::tungstenite::Message::*;

        tokio::task::spawn_local(async move {
            let mut should_reconnect = false;
            let mut should_shutdown = shutdown.subscribe();

            // Wrap the stream in a new TakeUntilIf stream, which will stop
            // processing once we receive something on the shutdown channel.
            let mut read_ws = Box::pin(read_ws.take_until_if(should_shutdown.recv().map(|_| true)));

            loop {
                // First make sure we actually received a message or
                // that the connection is closed.
                let message = match read_ws.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(err)) => {
                        warn!(
                            ?err,
                            ?conn_id,
                            "unable to read message from web-socket connection"
                        );
                        should_reconnect = true;
                        if let Err(e) = shutdown.send(()) {
                            warn!(?e, "unable to send shutdown signal");
                        };
                        break;
                    }
                    None => {
                        trace!(?conn_id, "stopping read loop; shutdown was invoked, or the connection is already closed");
                        if let Err(e) = shutdown.send(()) {
                            warn!(?e, "unable to send shutdown signal");
                        };
                        break;
                    }
                };

                // We are only interested in Binary and Close messages.
                match message {
                    Binary(msg) => {
                        trace!(?conn_id, "received binary message");
                        let message = match ServerMessage::deserialize_msgpack(msg) {
                            Ok(message) => message,
                            Err(err) => {
                                warn!(
                                    ?err,
                                    ?conn_id,
                                    "unable to deserialize msgpack encoded server message"
                                );
                                continue;
                            }
                        };

                        let op_id = message.op_id();
                        let result = service
                            .handle_message(message, tx_relay_messages.clone())
                            .await;
                        if let Err(err) = result {
                            warn!(
                                ?err,
                                ?conn_id,
                                ?op_id,
                                "service was unable to handle message"
                            );
                        };
                    }
                    Close(_) => {
                        trace!(?conn_id, "received close message");
                        if let Err(e) = shutdown.send(()) {
                            warn!(?e, "unable to send shutdown signal");
                        };
                        break;
                    }
                    Text(_) => error!(?conn_id, "Received Text"),
                    Ping(_) => trace!(?conn_id, "Received Ping"),
                    Pong(_) => trace!(?conn_id, "Received Pong"),
                }
            }

            should_reconnect
        })
        .await
    }

    /// Handle any outgoing relay messages (`rx_relay_messages`) by sending them
    /// to the outgoing web socket connection (`write_ws`).
    ///
    /// This will block until a message is broadcast on the `shutdown` channel.
    /// It can also exit if an error occurred during sending or receiving a
    /// message from the channel.
    async fn handle_write_loop(
        shutdown: Sender<()>,
        conn_id: String,
        rx_relay_messages: UnboundedReceiver<RelayMessage>,
        mut write_ws: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    ) {
        let mut should_shutdown = shutdown.subscribe();

        // Wrap the channel in a stream and then in a new TakeUntilIf stream,
        // which will stop processing once we receive something on the shutdown
        // channel.
        let rx_relay_messages = UnboundedReceiverStream::new(rx_relay_messages);
        let mut rx_relay_messages =
            Box::pin(rx_relay_messages.take_until_if(should_shutdown.recv().map(|_| true)));

        loop {
            select! {
                message = rx_relay_messages.next().fuse() => {
                    match message {
                        Some(message) => {
                            let op_id = message.op_id();

                            let mut buf = Vec::new();
                            if let Err(err) = message.serialize(&mut Serializer::new(&mut buf)) {
                                error!(
                                    ?err,
                                    ?conn_id,
                                    ?op_id,
                                    "unable to serialize message to msgpack"
                                );
                            };

                            if let Err(err) = write_ws.send(Message::Binary(buf)).await {
                                error!(
                                    ?err,
                                    ?conn_id,
                                    ?op_id,
                                    "unable to send message to relay"
                                );
                                break;
                            }

                            trace!(
                                ?conn_id,
                                "handle_command: sending message to relay complete"
                            );
                        }
                        None => {
                            trace!(?conn_id, "stopping write loop; shutdown was invoked, or the connection is already closed");
                            if let Err(e) = shutdown.send(()) {
                                warn!(?e, "unable to send shutdown signal");
                            };
                            break;
                        }
                    }
                }
                // Send a Ping message to the web socket connection if we have
                // not send anything for some amount of time.
                _ = sleep(WS_INACTIVITY_TIMEOUT).fuse() => {
                    if let Err(err) = write_ws.send(Message::Ping(b"ping".to_vec())).await {
                        warn!(?err, ?conn_id, "unable to send ping to server");
                    };
                }
            }
        }
    }

    async fn handle_message(
        &self,
        message: ServerMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        match message {
            ServerMessage::InvokeProxy(message) => {
                self.handle_invoke_proxy_message(message, reply).await
            }
        }
    }

    async fn handle_invoke_proxy_message(
        &self,
        message: InvokeProxyMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        let op_id = message.op_id;
        let data_source_name = message.data_source_name.as_str();
        debug!(
            "received a relay message for data source {}: {:?}",
            data_source_name, message
        );

        // Try to create the runtime for the given data source
        let data_source = match self.inner.data_sources.get(data_source_name) {
            Some(data_source) => data_source.clone(),
            None => {
                reply.send(RelayMessage::Error(ErrorMessage {
                    op_id,
                    message: format!(
                        "received relay message for unknown data source: {}",
                        data_source_name
                    ),
                }))?;
                return Ok(());
            }
        };
        let runtime = match self.create_runtime(data_source.ty()).await {
            Ok(runtime) => runtime,
            Err(e) => {
                reply.send(RelayMessage::Error(ErrorMessage {
                    op_id,
                    message: format!("error creating provider runtime: {:?}", e),
                }))?;
                return Ok(());
            }
        };

        trace!(?data_source, ?message, "Invoking provider");
        let DataSource::Prometheus(config) = data_source;
        let config = rmp_serde::to_vec(&config)?;
        let response_message = match runtime.invoke_raw(message.data, config).await {
            Ok(data) => {
                RelayMessage::InvokeProxyResponse(InvokeProxyResponseMessage { op_id, data })
            }
            Err(err) => {
                debug!(?err, "error invoking provider");
                RelayMessage::Error(ErrorMessage {
                    op_id,
                    message: format!("Provider runtime error: {:?}", err),
                })
            }
        };

        reply.send(response_message)?;

        Ok(())
    }

    async fn create_runtime(&self, data_source_type: &str) -> Result<Runtime> {
        let wasm_module: &[u8] = self
            .inner
            .wasm_modules
            .get(data_source_type)
            .unwrap_or_else(|| {
                panic!(
                    "should have loaded wasm module for provider {}",
                    data_source_type,
                )
            });

        compile_wasm(wasm_module)
    }
}

async fn load_wasm_modules(wasm_dir: &Path, data_sources: &DataSources) -> Result<WasmModuleMap> {
    let data_source_types: HashSet<String> = data_sources
        .0
        .values()
        .map(|d| d.ty().to_string())
        .collect();

    let mut wasm_modules = HashMap::new();
    for data_source_type in data_source_types.into_iter() {
        // Each provider's wasm module is found in the wasm_dir as provider_name.wasm
        let wasm_path = &wasm_dir.join(&format!("{}.wasm", &data_source_type));
        let wasm_module = fs::read(wasm_path)
            .await
            .with_context(|| format!("Error loading wasm file: {}", wasm_path.display()))?;

        // Make sure the wasm file can compile
        compile_wasm(&wasm_module).with_context(|| {
            format!(
                "Error compiling wasm file for provider: {}",
                &data_source_type
            )
        })?;

        debug!("loaded provider: {}", data_source_type);
        wasm_modules.insert(data_source_type, wasm_module);
    }

    Ok(wasm_modules)
}

fn compile_wasm(wasm_module: &[u8]) -> Result<Runtime> {
    // TODO can any of these objects be safely cloned between instances?
    let engine = Universal::new(Singlepass::default()).engine();
    let store = Store::new(&engine);
    let runtime = Runtime::new(store, wasm_module)?;
    Ok(runtime)
}
