use anyhow::{anyhow, Context, Result};
use fiberplane::protocols::core::{DataSource, DataSourceType};
use fp_provider_bindings::{
    Error, HttpRequestError, LegacyProviderRequest, LegacyProviderResponse,
};
use fp_provider_runtime::spec::Runtime;
use futures::{future::join_all, select, FutureExt};
use http::{Method, Request, Response, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Server};
use lazy_static::lazy_static;
use proxy_types::{
    DataSourceDetails, DataSourceDetailsOrType, DataSourceStatus, ErrorMessage, InvokeProxyMessage,
    InvokeProxyResponseMessage, RelayMessage, ServerMessage, SetDataSourcesMessage, Uuid,
};
use ring::digest::{digest, SHA256};
use serde_yaml::Value;
use std::collections::{HashMap, HashSet};
use std::{convert::Infallible, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc::{self, unbounded_channel, UnboundedSender};
use tokio::sync::{broadcast::Sender, watch};
use tokio::task::{spawn_blocking, LocalSet};
use tokio::time::{interval, timeout};
use tokio::{fs, runtime::Builder};
use tokio_tungstenite_reconnect::{Message, ReconnectingWebSocket};
use tracing::{debug, error, info, info_span, instrument, trace, Instrument, Span};
use url::Url;

pub(crate) type DataSources = HashMap<String, DataSource>;
pub(crate) type WasmModules = HashMap<DataSourceType, Result<Arc<Vec<u8>>, String>>;
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

lazy_static! {
    static ref STATUS_REQUEST: Vec<u8> = rmp_serde::to_vec(&LegacyProviderRequest::Status).unwrap();
}

#[derive(Clone, Debug)]
pub struct ProxyService {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    endpoint: Url,
    auth_token: String,
    pub(crate) data_sources: DataSources,
    wasm_modules: WasmModules,
    max_retries: u32,
    local_task_handler: SingleThreadTaskHandler,
    listen_address: Option<SocketAddr>,
    status_check_interval: Duration,
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
        status_check_interval: Duration,
    ) -> Self {
        let wasm_modules = load_wasm_modules(wasm_dir, &data_sources).await;

        ProxyService::new(
            fiberplane_endpoint,
            auth_token,
            wasm_modules,
            data_sources,
            max_retries,
            listen_address,
            status_check_interval,
        )
    }

    pub(crate) fn new(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_modules: WasmModules,
        data_sources: DataSources,
        max_retries: u32,
        listen_address: Option<SocketAddr>,
        status_check_interval: Duration,
    ) -> Self {
        ProxyService {
            inner: Arc::new(Inner {
                endpoint: fiberplane_endpoint,
                auth_token,
                data_sources,
                wasm_modules,
                max_retries,
                local_task_handler: SingleThreadTaskHandler::new(),
                listen_address,
                status_check_interval,
            }),
        }
    }

    #[instrument(err, skip_all)]
    pub async fn connect(&self, shutdown: Sender<()>) -> Result<()> {
        info!("connecting to fiberplane: {}", self.inner.endpoint);
        let (ws, mut conn_id_receiver) = self.connect_websocket().await?;
        conn_id_receiver.borrow_and_update();

        let span = if let Some(conn_id) = conn_id_receiver.borrow().clone() {
            info_span!("websocket", conn_id = conn_id.as_str())
        } else {
            info_span!("websocket", conn_id = tracing::field::Empty)
        };
        let _enter = span.enter();
        info!("connection established");

        // Update the conn_id if it changes
        tokio::spawn(
            async move {
                while conn_id_receiver.changed().await.is_ok() {
                    if let Some(conn_id) = conn_id_receiver.borrow().clone() {
                        Span::current().record("conn_id", &conn_id.as_str());
                    } else {
                        Span::current().record("conn_id", &tracing::field::Empty);
                    }
                }
            }
            .in_current_span(),
        );

        let (outgoing_sender, mut outgoing_receiver) = unbounded_channel::<RelayMessage>();

        // Health check endpoints
        let ws_clone = ws.clone();
        if let Some(listen_address) = self.inner.listen_address {
            tokio::spawn(
                async move {
                    if let Err(err) = serve_health_check_endpoints(listen_address, ws_clone).await {
                        // TODO should we shut the server down?
                        error!(?err, "Error serving health check endpoints");
                    }
                }
                .in_current_span(),
            );
        }

        // Spawn a task to send the data sources and their statuses to the relay
        let service = self.clone();
        let data_sources_sender = outgoing_sender.clone();
        let mut shutdown_clone = shutdown.subscribe();
        tokio::spawn(async move {
            let mut status_check_interval = interval(service.inner.status_check_interval);
            loop {
                select! {
                    // Note that the first tick returns immediately
                    _ = status_check_interval.tick().fuse() => {
                        let data_sources = service.get_data_sources().await;
                        debug!("sending data sources to relay: {:?}", data_sources);
                        let message = RelayMessage::SetDataSources(data_sources);
                        data_sources_sender.send(message).ok();
                    }
                    _ = shutdown_clone.recv().fuse() => {
                        // Let the relay know that all of these data sources are going offline
                        let data_sources = service.inner.data_sources.iter().map(|(name, data_source)| {
                            (name.clone(), DataSourceDetailsOrType::Details(DataSourceDetails {
                                ty: data_source.data_source_type(),
                                status: DataSourceStatus::Error("Proxy shut down".into())
                            }))
                        }).collect();
                        let message = RelayMessage::SetDataSources(data_sources);
                        data_sources_sender.send(message).ok();

                        break;
                    }
                }
            }
        });

        // Spawn a separate task for handling outgoing messages
        // so that incoming and outgoing do not interfere with one another
        let ws_clone = ws.clone();
        let mut shutdown_clone = shutdown.subscribe();
        tokio::spawn(
            async move {
                loop {
                    select! {
                        outgoing = outgoing_receiver.recv().fuse() => {
                            if let Some(message) = outgoing {
                                let trace_id = message.op_id();
                                let message = Message::Binary(message.serialize_msgpack());
                                let message_length = message.len();
                                match ws_clone.send(message).await {
                                    Ok(_) => debug!(?trace_id, %message_length, "sent response message"),
                                    Err(err) => error!(?err, "error sending outgoing message to WebSocket"),
                                }
                            }
                        },
                        _ = shutdown_clone.recv().fuse() => {
                            drop(ws_clone);
                            break;
                        }
                    }
                }
            }
            .in_current_span(),
        );

        loop {
            let outgoing_sender = outgoing_sender.clone();
            let mut shutdown = shutdown.subscribe();
            select! {
                incoming = ws.recv().fuse() => {
                    match incoming {
                        Some(Ok(Message::Binary(message))) => {
                            match ServerMessage::deserialize_msgpack(message) {
                                Ok(message) => {
                                    self.handle_message(message, outgoing_sender).await?;
                                },
                                Err(err) => {
                                    error!(?err, "Error deserializing MessagePack message");
                                }
                            }
                        },
                        Some(Err(err)) => {
                            error!(?err, "websocket error");
                            return Err(err.into())
                        },
                        None => {
                            debug!("websocket disconnected");
                            break;
                        }
                        message => debug!(?message, "ignoring websocket message of unexpected type")
                    }
                },
                _ = shutdown.recv().fuse() => {
                    trace!("shutdown");
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

    #[instrument(err, skip_all, fields(
        trace_id = ?message.op_id,
        data_source_name = ?message.data_source_name,
        message.data = %msgpack_to_json(&message.data)?
    ))]
    async fn handle_invoke_proxy_message(
        &self,
        message: InvokeProxyMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        let op_id = message.op_id;
        let data_source_name = message.data_source_name.as_str();
        debug!("handling relay message");

        // Try to create the runtime for the given data source
        let data_source = match self.inner.data_sources.get(data_source_name) {
            Some(data_source) => data_source.clone(),
            None => {
                error!("received relay message for unknown data source");
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

        let wasm_module = match &self.inner.wasm_modules[&data_source.data_source_type()] {
            Ok(wasm_module) => wasm_module,
            Err(message) => {
                return reply
                    .send(RelayMessage::Error(ErrorMessage {
                        op_id,
                        message: message.to_string(),
                    }))
                    .map_err(Into::into);
            }
        };
        let runtime = match compile_wasm(wasm_module.clone()).await {
            Ok(runtime) => runtime,
            Err(err) => {
                error!(?err, "error compiling wasm module");
                return reply
                    .send(RelayMessage::Error(ErrorMessage {
                        op_id,
                        message: err.to_string(),
                    }))
                    .map_err(Into::into);
            }
        };

        let config = match data_source {
            DataSource::Prometheus(config) => rmp_serde::to_vec(&config)?,
            DataSource::Elasticsearch(config) => rmp_serde::to_vec(&config)?,
            DataSource::Loki(config) => rmp_serde::to_vec(&config)?,
            DataSource::Proxy(_) => {
                error!("received relay message for proxy data source");
                reply.send(RelayMessage::Error(ErrorMessage {
                    op_id,
                    message: format!(
                        "cannot send a message from one proxy to another: {}",
                        data_source_name
                    ),
                }))?;
                return Ok(());
            }
            DataSource::Sentry(config) => rmp_serde::to_vec(&config)?,
        };

        let request = message.data;

        let task = Task {
            runtime,
            op_id,
            request,
            config,
            reply,
            span: Span::current(),
        };

        self.inner.local_task_handler.queue_task(task)?;

        Ok(())
    }

    /// Invoke each data source provider with the status request
    /// and only return the providers that returned an OK status.
    async fn get_data_sources(&self) -> SetDataSourcesMessage {
        join_all(
            self.inner
                .data_sources
                .iter()
                .map(|(name, data_source)| async move {
                    let status = match self.check_provider_status(name.clone()).await {
                        Ok(_) => DataSourceStatus::Connected,
                        Err(err) => DataSourceStatus::Error(err.to_string()),
                    };
                    (
                        name.clone(),
                        DataSourceDetailsOrType::Details(DataSourceDetails {
                            ty: data_source.data_source_type(),
                            status,
                        }),
                    )
                }),
        )
        .await
        .into_iter()
        .collect()
    }

    #[instrument(skip(self))]
    async fn check_provider_status(&self, data_source_name: String) -> Result<()> {
        let message = InvokeProxyMessage {
            op_id: Uuid::new_v4(),
            data_source_name,
            data: STATUS_REQUEST.clone(),
        };
        let (reply_sender, mut reply_receiver) = unbounded_channel();
        self.handle_invoke_proxy_message(message, reply_sender)
            .await?;
        let response = timeout(STATUS_REQUEST_TIMEOUT, reply_receiver.recv())
            .await
            .with_context(|| "timed out checking provider status")?
            .ok_or(anyhow!(
                "Did not receive a status response from the provider"
            ))?;
        let response = match response {
            RelayMessage::InvokeProxyResponse(response) => Ok::<_, anyhow::Error>(response),
            RelayMessage::Error(err) => {
                return Err(anyhow!("Error invoking provider: {}", err.message));
            }
            _ => {
                return Err(anyhow!("Unexpected response from provider: {:?}", response));
            }
        }?;
        match rmp_serde::from_slice(&response.data) {
            Ok(LegacyProviderResponse::StatusOk) => {
                debug!("provider status check returned OK");
                Ok(())
            }
            Ok(LegacyProviderResponse::Error {
                error: Error::UnsupportedRequest,
            }) => {
                debug!("provider does not support status request");
                Ok(())
            }
            // Try parsing the server response as a string so we can return a nicer message
            Ok(LegacyProviderResponse::Error {
                error:
                    Error::Http {
                        error:
                            HttpRequestError::ServerError {
                                status_code,
                                response,
                            },
                    },
            }) => {
                let response = response.to_vec();
                let response = if let Ok(response) = String::from_utf8(response.clone()) {
                    response
                } else {
                    format!("{:?}", response)
                };
                Err(anyhow!(
                    "Provider returned HTTP error: status={}, response={}",
                    status_code,
                    response
                ))
            }
            Ok(LegacyProviderResponse::Error { error }) => {
                Err(anyhow!("Provider returned an error: {:?}", error))
            }
            Err(err) => Err(anyhow!("Error deserializing provider response: {:?}", err)),
            _ => Err(anyhow!("Unexpected provider response: {:?}", response)),
        }
    }
}

async fn load_wasm_modules(
    wasm_dir: &Path,
    data_sources: &HashMap<String, DataSource>,
) -> WasmModules {
    let data_source_types: HashSet<DataSourceType> =
        data_sources.values().map(|d| d.into()).collect();

    join_all(
        data_source_types
            .into_iter()
            .map(|data_source_type| async move {
                // Each provider's wasm module is found in the wasm_dir as provider_name.wasm
                let wasm_path = &wasm_dir.join(&format!("{}.wasm", &data_source_type));
                let wasm_module = match fs::read(wasm_path).await {
                    Ok(wasm_module) => {
                        let wasm_module = Arc::new(wasm_module);
                        // Make sure the wasm file can compile
                        match compile_wasm(wasm_module.clone()).await {
                            Ok(_) => {
                                let hash = digest(&SHA256, wasm_module.as_ref());
                                info!(
                                    "loaded provider: {} (sha256 digest: {})",
                                    data_source_type,
                                    encode_hex(hash.as_ref())
                                );
                                Ok(wasm_module)
                            }
                            Err(err) => Err(format!(
                                "Error compiling wasm module {}: {}",
                                wasm_path.display(),
                                err
                            )),
                        }
                    }
                    Err(err) => Err(format!(
                        "Error loading wasm module {}: {}",
                        wasm_path.display(),
                        err
                    )),
                };
                (data_source_type, wasm_module)
            }),
    )
    .await
    .into_iter()
    .collect()
}

async fn compile_wasm(wasm_module: Arc<Vec<u8>>) -> Result<Runtime> {
    spawn_blocking(move || {
        let runtime = Runtime::new(wasm_module.as_ref())?;
        Ok(runtime)
    })
    .await?
}

/// This includes everything needed to handle a provider request
/// and reply with the response
struct Task {
    runtime: Runtime,
    op_id: Uuid,
    request: Vec<u8>,
    config: Vec<u8>,
    reply: UnboundedSender<RelayMessage>,
    span: Span,
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
        let _enter = task.span.enter();
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

        if let Err(err) = task.reply.send(response_message) {
            error!(?err, "unable to send response message to outgoing channel");
        };
    }
}

/// Listen on the given address and return a 200 for GET /
/// and either 200 or 502 for GET /health, depending on the WebSocket connection status
async fn serve_health_check_endpoints(addr: SocketAddr, ws: ReconnectingWebSocket) -> Result<()> {
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
                    trace!(http_status_code = %status.as_u16(), http_method = %request.method(), path = request.uri().path());

                    Ok::<_, Infallible>(Response::builder().status(status).body(body).unwrap())
                }
            }))
        }
    });

    debug!(?addr, "Serving health check endpoints");

    let server = Server::bind(&addr).serve(make_svc);
    Ok(server.await?)
}

fn msgpack_to_json(input: &[u8]) -> Result<String> {
    let mut deserializer = rmp_serde::Deserializer::new(input);
    let mut serializer = serde_json::Serializer::new(Vec::new());
    serde_transcode::transcode(&mut deserializer, &mut serializer)?;
    Ok(String::from_utf8(serializer.into_inner())?)
}

fn encode_hex(input: &[u8]) -> String {
    input
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join("")
}

pub fn parse_data_sources_yaml(yaml: &str) -> Result<DataSources> {
    match serde_yaml::from_str(yaml) {
        Ok(data_sources) => Ok(data_sources),
        // Try parsing the old format that has a separate options key
        Err(err) => match serde_yaml::from_str::<Value>(yaml) {
            Ok(Value::Mapping(map)) => {
                let value = map
                    .into_iter()
                    .map(|(key, mut value)| {
                        if let Value::Mapping(ref mut value) = value {
                            // Flatten the options into the top level map
                            if let Some(Value::Mapping(options)) =
                                value.remove(&Value::String("options".to_string()))
                            {
                                value.extend(options);
                            }
                        }
                        (key, value)
                    })
                    .collect();
                let data_sources = serde_yaml::from_value(Value::Mapping(value))?;
                Ok(data_sources)
            }
            Ok(_) => Err(anyhow!(
                "Unable to parse data sources YAML: Expected a mapping"
            )),
            Err(_) => Err(anyhow!("Unable to parse data sources YAML: {}", err)),
        },
    }
}
