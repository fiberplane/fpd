use crate::data_sources::{DataSource, DataSources};
use anyhow::{anyhow, Context, Result};
use fp_provider_runtime::Runtime;
use futures::select;
use futures::FutureExt;
use http::Method;
use http::StatusCode;
use http::{Request, Response};
use hyper::{
    service::{make_service_fn, service_fn},
    Body, Server,
};
use proxy_types::{
    ErrorMessage, InvokeProxyMessage, InvokeProxyResponseMessage, RelayMessage, ServerMessage,
    SetDataSourcesMessage, Uuid,
};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::runtime::Builder;
use tokio::sync::broadcast::Sender;
use tokio::sync::mpsc::{self, unbounded_channel, UnboundedSender};
use tokio::sync::watch;
use tokio::task::LocalSet;
use tokio_tungstenite_reconnect::{Message, ReconnectingWebSocket};
use tracing::{debug, error, info, trace};
use url::Url;

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
    local_task_handler: SingleThreadTaskHandler,
    listen_address: Option<SocketAddr>,
}

impl ProxyService {
    /// Load the provider wasm files from the given directory and create a new Proxy instance
    pub async fn init(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_dir: &Path,
        data_sources: DataSources,
        max_retries: u32,
        listen_address: Option<SocketAddr>,
    ) -> Result<Self> {
        let wasm_modules = load_wasm_modules(wasm_dir, &data_sources).await?;
        Ok(ProxyService::new(
            fiberplane_endpoint,
            auth_token,
            wasm_modules,
            data_sources,
            max_retries,
            listen_address,
        ))
    }

    pub(crate) fn new(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_modules: WasmModuleMap,
        data_sources: DataSources,
        max_retries: u32,
        listen_address: Option<SocketAddr>,
    ) -> Self {
        ProxyService {
            inner: Arc::new(Inner {
                endpoint: fiberplane_endpoint,
                auth_token,
                wasm_modules,
                data_sources,
                max_retries,
                local_task_handler: SingleThreadTaskHandler::new(),
                listen_address,
            }),
        }
    }

    pub async fn connect(&self, shutdown: Sender<()>) -> Result<()> {
        info!("connecting to fiberplane");
        let (ws, mut conn_id_receiver) = self.connect_websocket().await?;
        let mut conn_id: Option<String> = conn_id_receiver.borrow().clone();
        info!(?conn_id, "connection established");

        // Send the list of data sources to the relay
        let data_sources: SetDataSourcesMessage = self
            .inner
            .data_sources
            .iter()
            .map(|(name, data_source)| (name.clone(), data_source.into()))
            .collect();
        debug!(
            ?conn_id,
            "sending data sources to relay: {:?}", data_sources
        );
        let message = RelayMessage::SetDataSources(data_sources);
        let message = Message::Binary(message.serialize_msgpack());
        ws.send(message).await?;

        let (reply_sender, mut reply_receiver) = unbounded_channel::<RelayMessage>();

        // Health check endpoints
        let ws_clone = ws.clone();
        if let Some(listen_address) = self.inner.listen_address {
            tokio::spawn(async move {
                if let Err(err) = serve_health_check_endpionts(listen_address, ws_clone).await {
                    // TODO should we shut the server down?
                    error!(?err, "Error serving health check endpoints");
                }
            });
        }

        // Spawn a separate task for handling outgoing messages
        // so that incoming and outgoing do not interfere with one another
        let mut conn_id_receiver_clone = conn_id_receiver.clone();
        conn_id_receiver_clone.borrow_and_update();
        let ws_clone = ws.clone();
        let mut shutdown_clone = shutdown.subscribe();
        tokio::spawn(async move {
            let mut conn_id: Option<String> = conn_id_receiver_clone.borrow().clone();
            loop {
                select! {
                    outgoing = reply_receiver.recv().fuse() => {
                        if let Some(message) = outgoing {
                            trace!(?conn_id, ?message, "sending outgoing message");
                            let message = Message::Binary(message.serialize_msgpack());
                            if let Err(err) = ws_clone.send(message).await {
                                error!(?conn_id, ?err, "error sending outgoing message to WebSocket");
                            }
                        }
                    },
                    _ = conn_id_receiver_clone.changed().fuse() => {
                        conn_id = conn_id_receiver_clone.borrow().clone();
                    }
                    _ = shutdown_clone.recv().fuse() => {
                        drop(ws_clone);
                        break;
                    }
                }
            }
        });

        loop {
            let reply_sender = reply_sender.clone();
            let mut shutdown = shutdown.subscribe();
            select! {
                incoming = ws.recv().fuse() => {
                    match incoming {
                        Some(Ok(Message::Binary(message))) => {
                            match ServerMessage::deserialize_msgpack(message) {
                                Ok(message) => {
                                    trace!(?conn_id, ?message, "got incoming message");
                                    self.handle_message(message, reply_sender).await?;
                                },
                                Err(err) => {
                                    error!(?conn_id, ?err, "Error deserializing MessagePack message");
                                }
                            }
                        },
                        Some(Err(err)) => {
                            error!(?conn_id, ?err, "websocket error");
                            return Err(err.into())
                        },
                        None => {
                            debug!(?conn_id, "websocket connection closed");
                            break;
                        }
                        message => debug!(?conn_id, "ignoring websocket message of unexpected type {:?}", message)
                    }
                },
                _ = conn_id_receiver.changed().fuse() => {
                    let new_id = conn_id_receiver.borrow().clone();
                    if conn_id != new_id {
                        trace!("conn_id {:?} changed to {:?}", conn_id, new_id);
                    }
                    conn_id = new_id;
                }
                _ = shutdown.recv().fuse() => {
                    trace!(?conn_id, "shutdown");
                    drop(ws);
                    break;
                }
            }
        }
        Ok(())
    }

    /// Connects to a web-socket server and returns the connection id and the
    /// web-socket stream.
    async fn connect_websocket(
        &self,
    ) -> Result<(ReconnectingWebSocket, watch::Receiver<Option<String>>)> {
        // Create a request object. If this fails there is no point in
        // retrying so just return the error object.
        let request = http::Request::builder()
            .uri(self.inner.endpoint.as_str())
            .header("fp-auth-token", self.inner.auth_token.clone())
            .body(())?;

        let (conn_id_sender, conn_id_receiver) = watch::channel(None);
        let ws = ReconnectingWebSocket::builder(request)?
            .max_retries(self.inner.max_retries)
            .connect_response_handler(move |response| {
                let conn_id = response
                    .headers()
                    .get("x-fp-conn-id")
                    .and_then(|id| id.to_str().map(|hv| hv.to_owned()).ok());
                if *conn_id_sender.subscribe().borrow() != conn_id {
                    conn_id_sender.send_replace(conn_id);
                }
            })
            .build();

        ws.connect().await?;

        if conn_id_receiver.borrow().is_some() {
            Ok((ws, conn_id_receiver))
        } else {
            Err(anyhow!("no connection id was returned"))
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
        let request = message.data;

        let task = Task {
            runtime,
            op_id,
            request,
            config,
            reply,
        };

        self.inner.local_task_handler.queue_task(task)?;

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
    let runtime = Runtime::new(wasm_module)?;
    Ok(runtime)
}

/// This includes everything needed to handle a provider request
/// and reply with the response
struct Task {
    runtime: Runtime,
    op_id: Uuid,
    request: Vec<u8>,
    config: Vec<u8>,
    reply: UnboundedSender<RelayMessage>,
}

/// A SingleThreadTaskHandler will make sure that all tasks that are run on it
/// will be run in a single threaded tokio runtime. This allows us to execute
/// work that has !Send.
///
/// (This is necessary for the Proxy because the Wasmer runtime's exported
/// functions are !Send and thus cannot be used with the normal tokio::spawn.)
#[derive(Debug, Clone)]
struct SingleThreadTaskHandler {
    tx: mpsc::UnboundedSender<Task>,
}

impl SingleThreadTaskHandler {
    /// Spawn a new thread to handle the given tasks
    pub fn new() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Task>();

        // Spawn a new OS thread that will run a single-threaded Tokio runtime to handle messages
        std::thread::spawn(move || {
            let rt = Builder::new_current_thread().enable_all().build().unwrap();
            let local = LocalSet::new();
            local.block_on(&rt, async move {
                // Process all messages from the channel by spawning a local task
                // (on this same thread) that will call the provider
                while let Some(task) = rx.recv().await {
                    // Spawn a task so that the runtime will alternate between
                    // accepting new tasks and running each existing one to completion
                    tokio::task::spawn_local(Self::invoke_proxy_and_send_response(task));
                }
                debug!("SingleThreadTaskHandler was dropped, no more messages to process");
                // If the while loop returns, then all the LocalSpawner
                // objects have have been dropped.
            });
            debug!("SingleThreadTaskHandler spawned thread shutting down");
        });

        Self { tx }
    }

    pub fn queue_task(&self, task: Task) -> Result<()> {
        self.tx
            .send(task)
            .map_err(|err| anyhow!("unable to queue task {}", err))
    }

    async fn invoke_proxy_and_send_response(task: Task) {
        let op_id = task.op_id;
        let runtime = task.runtime;

        let response_message = match runtime.invoke_raw(task.request, task.config).await {
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

        if let Err(send_err) = task.reply.send(response_message) {
            error!(?send_err, "unable to send error to relay");
        };
    }
}

/// Listen on the given address and return a 200 for GET /
/// and either 200 or 502 for GET /health, depending on the WebSocket connection status
async fn serve_health_check_endpionts(addr: SocketAddr, ws: ReconnectingWebSocket) -> Result<()> {
    let make_svc = make_service_fn(move |_conn| {
        let ws = ws.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |request: Request<Body>| {
                let ws = ws.clone();
                async move {
                    let (status, body) = match (request.method(), request.uri().path()) {
                        (&Method::GET, "/") | (&Method::GET, "") => (
                            StatusCode::OK,
                            Body::from("Hi, I'm your friendly neighborhood proxy.".to_string()),
                        ),
                        (&Method::GET, "/health") => {
                            if ws.is_connected() {
                                (StatusCode::OK, Body::from("Connected".to_string()))
                            } else {
                                (
                                    StatusCode::BAD_GATEWAY,
                                    Body::from("Disconnected".to_string()),
                                )
                            }
                        }
                        (_, _) => (StatusCode::NOT_FOUND, Body::empty()),
                    };

                    Ok::<_, Infallible>(Response::builder().status(status).body(body).unwrap())
                }
            }))
        }
    });

    debug!(?addr, "Serving health check endpoints");

    let server = Server::bind(&addr).serve(make_svc);
    Ok(server.await?)
}
