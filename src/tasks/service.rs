use super::metrics::{metrics_export, CONCURRENT_QUERIES, QUERIES_DURATION_SECONDS, QUERIES_TOTAL};
use super::tokio_tungstenite_reconnect::ReconnectingWebSocket;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use fiberplane::base64uuid::Base64Uuid;
use fiberplane::models::providers::{Error, STATUS_MIME_TYPE, STATUS_QUERY_TYPE};
use fiberplane::models::{data_sources::DataSourceStatus, names::Name, proxies::*};
use fiberplane::provider_bindings::Blob;
use fiberplane::provider_runtime::spec::{types::ProviderRequest, Runtime};
use futures::{future::join_all, select, FutureExt};
use http::{Method, Response, StatusCode};
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use status_check::DataSourceCheckTask;
use std::collections::HashMap;
use std::{convert::Infallible, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex;
use tokio::sync::{broadcast::Sender, watch};
use tokio::{fs, time::interval};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument, Span};
use url::Url;

mod bindings;
mod status_check;
#[cfg(test)]
mod tests;

const STATUS_CHECK_VERSION: u8 = 2;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProxyDataSource {
    pub name: Name,
    pub provider_type: String,
    pub config: Map<String, Value>,
    pub description: Option<String>,
}

pub(crate) type WasmModules = HashMap<String, Result<Runtime, Error>>;

static STATUS_REQUEST_V2: Lazy<Vec<u8>> = Lazy::new(|| {
    rmp_serde::to_vec_named(
        &ProviderRequest::builder()
            .query_type(STATUS_QUERY_TYPE)
            .query_data(
                Blob::builder()
                    .data(Vec::new())
                    .mime_type(STATUS_MIME_TYPE)
                    .build(),
            )
            .config(Value::Null)
            .build(),
    )
    .unwrap()
});

#[derive(Clone)]
pub struct ProxyService {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    endpoint: Url,
    token: String,
    pub(crate) data_sources: HashMap<Name, ProxyDataSource>,
    data_sources_state: Mutex<HashMap<Name, UpsertProxyDataSource>>,
    wasm_modules: WasmModules,
    max_retries: u32,
    listen_address: Option<SocketAddr>,
    status_check_interval: Duration,
}

impl ProxyService {
    /// Load the provider wasm files from the given directory and create a new Proxy instance
    pub async fn init(
        api_base: Url,
        token: ProxyToken,
        wasm_dir: &Path,
        data_sources: Vec<ProxyDataSource>,
        max_retries: u32,
        listen_address: Option<SocketAddr>,
        status_check_interval: Duration,
    ) -> Self {
        let data_sources: HashMap<Name, ProxyDataSource> = data_sources
            .into_iter()
            .map(|data_source| (data_source.name.clone(), (data_source)))
            .collect();
        let provider_types = data_sources
            .values()
            .map(|ds| ds.provider_type.clone())
            .collect();
        let wasm_modules = load_wasm_modules(wasm_dir, provider_types).await;

        ProxyService::new(
            api_base,
            token,
            wasm_modules,
            data_sources,
            max_retries,
            listen_address,
            status_check_interval,
        )
    }

    pub(crate) fn new(
        api_base: Url,
        token: ProxyToken,
        wasm_modules: WasmModules,
        data_sources: HashMap<Name, ProxyDataSource>,
        max_retries: u32,
        listen_address: Option<SocketAddr>,
        status_check_interval: Duration,
    ) -> Self {
        let mut endpoint = api_base
            .join(&format!(
                "/api/workspaces/{}/proxies/{}/ws",
                token.workspace_id, token.proxy_name
            ))
            .expect("Invalid Fiberplane endpoint");
        if endpoint.scheme().starts_with("http") {
            endpoint
                .set_scheme(&endpoint.scheme().replace("http", "ws"))
                .unwrap();
        }

        ProxyService {
            inner: Arc::new(Inner {
                endpoint,
                token: token.token,
                data_sources,
                data_sources_state: Default::default(),
                wasm_modules,
                max_retries,
                listen_address,
                status_check_interval,
            }),
        }
    }

    /// Return a suitable ProxyMessage payload informing of the current
    /// state of all data sources.
    #[instrument(skip_all)]
    pub async fn to_data_sources_proxy_message(&self) -> ProxyMessage {
        ProxyMessage::new_set_data_sources_notification(
            self.inner
                .data_sources_state
                .lock()
                .await
                .values()
                .cloned()
                .collect(),
        )
    }

    /// Delegate to access the current state of a single data source by name
    #[instrument(err, skip(self))]
    pub async fn data_source_state(&self, name: &Name) -> Result<UpsertProxyDataSource> {
        self.inner
            .data_sources_state
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("{name} is an unknown data source for this proxy"))
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
                        Span::current().record("conn_id", conn_id.as_str());
                    } else {
                        Span::current().record("conn_id", &tracing::field::Empty);
                    }
                }
            }
            .in_current_span(),
        );

        let (outgoing_sender, mut outgoing_receiver) = unbounded_channel::<ProxyMessage>();

        // Health check endpoints
        let ws_clone = ws.clone();
        if let Some(listen_address) = self.inner.listen_address {
            let tcp_listener = TcpListener::bind(listen_address).await?;

            tokio::spawn(
                async move {
                    if let Err(err) = serve_health_check_endpoints(tcp_listener, ws_clone).await {
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
        let (data_source_check_task_sender, mut data_source_check_task_receiver) =
            unbounded_channel::<DataSourceCheckTask>();
        let data_source_check_task_tx_too = data_source_check_task_sender.clone();
        tokio::spawn(async move {
            let mut status_check_interval = interval(service.inner.status_check_interval);
            loop {
                select! {
                    // Note that the first tick returns immediately
                    _ = status_check_interval.tick().fuse() => {
                        service.update_all_data_sources(data_source_check_task_sender.clone()).await;
                        // Update data sources will both try to connect and automatically queue individual retries to the
                        // `data_source_check_task_receiver` queue with the correct delay if necessary
                        let message = service.to_data_sources_proxy_message().await;
                        debug!("sending data sources to relay: {:?}", message);
                        data_sources_sender.send(message).ok();
                    }
                    // A status check for a data source failed, and
                    // the queued retry will arrive here.
                    task = data_source_check_task_receiver.recv().fuse() => {
                        if let Some(task) = task {
                            let source_name = task.name().clone();
                            // Update data source will both try to connect and automatically queue a retry
                            // to the same queue as here (`data_source_check_task_receiver`)
                            // with the correct delay if necessary
                            service.update_data_source(task, data_source_check_task_tx_too.clone()).await;

                            // Log the result of the new update attempt
                            match service.data_source_state(&source_name).await {
                                Ok(attempt) => debug!("retried connecting to {}: new status is: {:?}", source_name, attempt),
                                Err(err) => warn!("retried connecting to {}: {err}", source_name),
                            }

                            let message = service.to_data_sources_proxy_message().await;
                            debug!("sending data sources to relay: {:?}", message);
                            data_sources_sender.send(message).ok();
                        }
                    }
                    _ = shutdown_clone.recv().fuse() => {
                        // Let the relay know that all of these data sources are going offline
                        let data_sources = service
                            .inner
                            .data_sources
                            .values()
                            .map(|data_source| {
                                let mut upsert = UpsertProxyDataSource::builder()
                                    .name(data_source.name.clone())
                                    .provider_type(data_source.provider_type.clone())
                                    .protocol_version(STATUS_CHECK_VERSION)
                                    .status(DataSourceStatus::Error(Error::ProxyDisconnected))
                                    .build();
                                upsert.description.clone_from(&data_source.description);
                                upsert
                            })
                            .collect();
                        let message = ProxyMessage::new_set_data_sources_notification(data_sources);
                        data_sources_sender.send(message).ok();

                        break;
                    }
                }
            }
        }.in_current_span());

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

        // NOTE: a Span Guard is unsafe to hold across awaits, and can
        // mess up the reported traces.
        //
        // All code above carefully avoids
        // calling await outside of tokio::spawned-futures so it's safe
        // to have the guard in scope, but past this point it's not
        // safe anymore, so we manually drop the guard.
        //
        // tokio::select! actually hides top level awaits.
        //
        // Ref: https://docs.rs/tracing/latest/tracing/struct.Span.html#method.enter
        //      https://docs.rs/tokio/latest/tokio/macro.select.html
        drop(_enter);

        loop {
            let outgoing_sender = outgoing_sender.clone();
            let mut shutdown = shutdown.subscribe();
            select! {
                incoming = ws.recv().fuse() => {
                    match incoming {
                        Some(Ok(Message::Binary(message))) => {
                            match ServerMessage::deserialize_msgpack(message) {
                                Ok(message) => {
                                    let service = self.clone();
                                    tokio::spawn(async move {
                                        if let Err(err) = service.handle_message(message, outgoing_sender).await {
                                            error!("Error handling message: {:?}", err);
                                        };
                                    }.instrument(span.clone()));
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
            .header("fp-auth-token", self.inner.token.clone())
            .body(())?;

        let (conn_id_sender, conn_id_receiver) = watch::channel(None);
        let ws = ReconnectingWebSocket::builder(request)?
            .max_retries(self.inner.max_retries)
            .connect_response_handler(move |response| {
                let conn_id = response
                    .headers()
                    .get("fp-conn-id")
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

    #[instrument(skip_all, fields(
        trace_id = ?message.op_id,
        data_source_name = ?message.data_source_name,
    ))]
    async fn handle_message(
        &self,
        message: ServerMessage,
        reply: UnboundedSender<ProxyMessage>,
    ) -> Result<()> {
        let response = self.handle_message_inner(message).await?;

        reply
            .send(response)
            .context("Error sending response to relay")?;

        Ok(())
    }

    #[instrument(skip_all, fields(
        trace_id = ?message.op_id,
        data_source_name = ?message.data_source_name,
    ))]
    async fn handle_message_inner(&self, message: ServerMessage) -> Result<ProxyMessage> {
        debug!("handling relay message");
        let op_id = message.op_id().ok_or_else(|| Error::Deserialization {
            message: "Incoming message is expecting an operation ID".to_string(),
        })?;

        // Currently we only support version 2 of the server message protocol
        if message.protocol_version != 2 {
            return Ok(ProxyMessage::new_error_response(
                Error::Invocation {
                    message: format!("unsupported version: {}", message.protocol_version),
                },
                op_id,
            ));
        }

        // Try to create the runtime for the given data source
        let data_source = match self.inner.data_sources.get(&message.data_source_name) {
            Some(data_source) => data_source.clone(),
            None => {
                error!("received relay message for unknown data source");

                return Ok(ProxyMessage::new_error_response(Error::NotFound, op_id));
            }
        };

        let runtime = match self
            .inner
            .wasm_modules
            .get(&data_source.provider_type)
            .unwrap()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                return Ok(ProxyMessage::new_error_response(error.clone(), op_id));
            }
        };

        // Track metrics
        let protocol_version = message.protocol_version.to_string();
        let labels = [
            protocol_version.as_str(),
            &data_source.provider_type,
            &data_source.name,
        ];
        QUERIES_TOTAL.with_label_values(&labels).inc();
        CONCURRENT_QUERIES.with_label_values(&labels).inc();
        let timer = QUERIES_DURATION_SECONDS
            .with_label_values(&labels)
            .start_timer();

        debug!(%protocol_version, %data_source.provider_type, %message.op_id, "Calling provider with {:?}", message.payload);
        let response = match message.payload {
            ServerMessagePayload::Invoke(message) => {
                self.handle_invoke_proxy_message(message, runtime, &data_source, op_id)
                    .await
            }
            ServerMessagePayload::CreateCells(message) => {
                self.handle_create_cells_proxy_message(message, runtime, &data_source, op_id)
            }
            ServerMessagePayload::ExtractData(message) => {
                self.handle_extract_data_proxy_message(message, runtime, &data_source, op_id)
            }
            ServerMessagePayload::GetConfigSchema(message) => {
                self.handle_config_schema_proxy_message(message, runtime, &data_source, op_id)
            }
            ServerMessagePayload::GetSupportedQueryTypes(message) => {
                self.handle_supported_query_types(message, runtime, &data_source, op_id)
                    .await
            }
            payload => ProxyMessage::new_error_response(
                Error::Invocation {
                    message: format!("unsupported payload: ({:?})", payload),
                },
                op_id,
            ),
        };

        CONCURRENT_QUERIES.with_label_values(&labels).dec();
        timer.observe_duration();

        Ok(response)
    }

    #[instrument(skip_all, fields(
        trace_id = ?op_id,
        data_source_name = ?data_source.name,
    ))]
    async fn handle_invoke_proxy_message(
        &self,
        message: InvokeRequest,
        runtime: &Runtime,
        data_source: &ProxyDataSource,
        op_id: Base64Uuid,
    ) -> ProxyMessage {
        let result =
            bindings::invoke_provider_v2(runtime, message.data, data_source.config.clone()).await;
        match result {
            Ok(response) => ProxyMessage::new_invoke_proxy_response(response, op_id),
            Err(error) => ProxyMessage::new_error_response(error, op_id),
        }
    }

    #[instrument(skip_all, fields(
        trace_id = ?op_id,
        data_source_name = ?data_source.name,
    ))]
    fn handle_create_cells_proxy_message(
        &self,
        message: CreateCellsRequest,
        runtime: &Runtime,
        data_source: &ProxyDataSource,
        op_id: Base64Uuid,
    ) -> ProxyMessage {
        info!("Calling create_cells on {}", data_source.name);
        let result = bindings::create_cells(runtime, &message.query_type, message.response);
        match result {
            Ok(cells) => ProxyMessage::new_create_cells_response(cells, op_id),
            Err(error) => ProxyMessage::new_error_response(error, op_id),
        }
    }

    #[instrument(skip_all, fields(
        trace_id = ?op_id,
        data_source_name = ?data_source.name,
    ))]
    fn handle_extract_data_proxy_message(
        &self,
        message: ExtractDataRequest,
        runtime: &Runtime,
        data_source: &ProxyDataSource,
        op_id: Base64Uuid,
    ) -> ProxyMessage {
        info!("Calling extract_data on {}", data_source.name);
        let result = bindings::extract_data(
            runtime,
            message.response,
            &message.mime_type,
            &message.query,
        );
        match result {
            Ok(data) => ProxyMessage::new_extract_data_response(data, op_id),
            Err(error) => ProxyMessage::new_error_response(error, op_id),
        }
    }

    #[instrument(skip_all, fields(
        trace_id = ?op_id,
        data_source_name = ?data_source.name,
    ))]
    fn handle_config_schema_proxy_message(
        &self,
        _message: GetConfigSchemaRequest,
        runtime: &Runtime,
        data_source: &ProxyDataSource,
        op_id: Base64Uuid,
    ) -> ProxyMessage {
        info!("Calling get_config_schema on {}", data_source.name);
        let result = bindings::get_config_schema(runtime);
        match result {
            Ok(schema) => ProxyMessage::new_config_schema_response(schema, op_id),
            Err(error) => ProxyMessage::new_error_response(error, op_id),
        }
    }

    #[instrument(skip_all, fields(
        trace_id = ?op_id,
        data_source_name = ?data_source.name,
    ))]
    async fn handle_supported_query_types(
        &self,
        _message: GetSupportedQueryTypesRequest,
        runtime: &Runtime,
        data_source: &ProxyDataSource,
        op_id: Base64Uuid,
    ) -> ProxyMessage {
        info!("Calling supported_query_types on {}", data_source.name);
        let result = bindings::get_supported_query_types(runtime, &data_source.config).await;
        match result {
            Ok(queries) => ProxyMessage::new_supported_query_types_response(queries, op_id),
            Err(error) => ProxyMessage::new_error_response(error, op_id),
        }
    }

    /// Try to connect to a data source according to task
    ///
    /// On success, return the update message
    /// On failure, return the update message _and_ queue the next retry task to the
    ///     individual_check_task_queue_tx sender if the retry policy allows for a new
    ///     retry.
    async fn update_data_source(
        &self,
        task: DataSourceCheckTask,
        individual_check_task_queue_tx: UnboundedSender<DataSourceCheckTask>,
    ) {
        let update = self
            .inner
            .data_sources
            .iter()
            .find(|(name, _)| *name == task.name())
            .map(|(name, data_source)| async move {
                let response = self.check_provider_status_v2(name.clone()).await;

                let status = match response {
                    Ok(_) => DataSourceStatus::Connected,
                    Err(ref err) => DataSourceStatus::Error(err.clone()),
                };

                if let Some((delay, task)) = task.next() {
                    if response.is_err() {
                        error!(
                            "error connecting to data source: {name}, retrying in {}s",
                            delay.as_secs()
                        );
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            individual_check_task_queue_tx.send(task)
                        });
                    }
                }

                let mut upsert = UpsertProxyDataSource::builder()
                    .name(name.clone())
                    .provider_type(data_source.provider_type.clone())
                    .protocol_version(STATUS_CHECK_VERSION)
                    .status(status)
                    .build();
                upsert.description.clone_from(&data_source.description);
                upsert
            })
            .unwrap()
            .await;

        self.inner
            .data_sources_state
            .lock()
            .await
            .insert(update.name.clone(), update);
    }

    async fn update_all_data_sources(
        &self,
        to_check_task_queue: UnboundedSender<DataSourceCheckTask>,
    ) {
        join_all(
            self.inner
                .data_sources
                .iter()
                .zip(std::iter::repeat(to_check_task_queue))
                .map(|((name, _), to_check_task_queue)| async move {
                    let task = DataSourceCheckTask::new(
                        name.clone(),
                        self.inner.status_check_interval,
                        Duration::from_secs(10),
                        1.5,
                    );
                    self.update_data_source(task, to_check_task_queue.clone())
                        .await
                }),
        )
        .await;
    }

    #[instrument(skip(self))]
    async fn check_provider_status_v2(&self, data_source_name: Name) -> Result<(), Error> {
        debug!(
            "Using protocol v2 to check provider status: {}",
            &data_source_name
        );
        let message = ServerMessage::new_invoke_proxy_request(
            STATUS_REQUEST_V2.clone(),
            data_source_name.clone(),
            2,
            Base64Uuid::new(),
        );

        match self
            .handle_message_inner(message)
            .await
            .expect("The handler only fails if message has no op_id")
            .payload
        {
            ProxyMessagePayload::InvokeProxyResponse(response) => {
                let result: Result<Blob, Error> =
                    rmp_serde::from_slice(&response.data).map_err(|err| {
                        Error::Deserialization {
                            message: format!("Error deserializing provider response: {err}"),
                        }
                    })?;
                result?;
                Ok(())
            }
            ProxyMessagePayload::Error(err) => Err(err.error),
            message => Err(Error::Invocation {
                message: format!("Unexpected provider response: {message:?}"),
            }),
        }
    }
}

async fn load_wasm_modules(wasm_dir: &Path, provider_types: Vec<String>) -> WasmModules {
    let runtimes = join_all(provider_types.iter().map(|data_source_type| async move {
        // Each provider's wasm module is found in the wasm_dir as data_source_type.wasm
        let wasm_path = &wasm_dir.join(format!("{}.wasm", &data_source_type));
        let wasm_module = fs::read(wasm_path).await.map_err(|err| {
            error!("Error reading wasm file: {} {}", wasm_path.display(), err);
            Error::Invocation {
                message: format!("Error reading wasm file: {err}"),
            }
        })?;
        Runtime::new(wasm_module).map_err(|err| {
            error!("Error compiling wasm module: {}", err);
            Error::Invocation {
                message: format!("Error compiling wasm module: {err}"),
            }
        })
    }))
    .await;

    provider_types.into_iter().zip(runtimes).collect()
}

/// Listen on the given address and return a 200 for GET /
/// and either 200 or 502 for GET /health, depending on the WebSocket connection status
async fn serve_health_check_endpoints(
    tcp_listener: TcpListener,
    ws: ReconnectingWebSocket,
) -> Result<()> {
    loop {
        let (stream, _) = tcp_listener
            .accept()
            .await
            .expect("unable to accept connection");

        let ws = ws.clone();

        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(
                    io,
                    service_fn(move |request| {
                    let ws = ws.clone();
                    async move {
                        let (status, body) = match (request.method(), request.uri().path()) {
                            (&Method::GET, "/") | (&Method::GET, "") => (
                                StatusCode::OK,
                                "Hi, I'm your friendly neighborhood proxy.".to_string(),
                            ),
                            (&Method::GET, "/health") => {
            if ws.is_connected() {
                                    (StatusCode::OK, "Connected".to_string())
                                } else {
                                    (
                                        StatusCode::BAD_GATEWAY,
                                        "Disconnected".to_string(),
                                    )
                                }
                            }
                            (&Method::GET, "/metrics") => match metrics_export() {
                                Ok(metrics) => (StatusCode::OK, metrics),
                                Err(err) => (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    format!("Error exporting metrics: {err}"),
                                ),
                            },
                            (_, _) => (StatusCode::NOT_FOUND, "Not Found".to_string()),
                        };
                        trace!(http_status_code = %status.as_u16(), http_method = %request.method(), path = request.uri().path());

                        Ok::<_, Infallible>(Response::builder().status(status).body(Full::new(Bytes::from(body))).unwrap())
                    }
                }))
                .await
                {
                    error!("Error serving connection: {:?}", err);
                }
        });
    }
}
