use super::metrics::{metrics_export, CONCURRENT_QUERIES, QUERIES_DURATION_SECONDS, QUERIES_TOTAL};
use super::tokio_tungstenite_reconnect::ReconnectingWebSocket;
use anyhow::{anyhow, Context, Result};
use fiberplane::base64uuid::Base64Uuid;
use fiberplane::models::providers::{Error, STATUS_MIME_TYPE, STATUS_QUERY_TYPE};
use fiberplane::models::{data_sources::DataSourceStatus, names::Name, proxies::*};
use fiberplane::provider_bindings::{
    Blob, HttpRequestError, LegacyProviderRequest, LegacyProviderResponse,
};
use fiberplane::provider_runtime::spec::{types::ProviderRequest, Runtime};
use futures::{future::join_all, select, FutureExt};
use http::{Method, Request, Response, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Server};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::{convert::Infallible, net::SocketAddr, path::Path, sync::Arc, time::Duration};
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

use status_check::DataSourceCheckTask;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProxyDataSource {
    pub name: Name,
    pub provider_type: String,
    pub config: Map<String, Value>,
    pub description: Option<String>,
}

pub(crate) type WasmModules = HashMap<String, Result<Runtime, Error>>;
const V1_PROVIDERS: &[&str] = &["elasticsearch", "loki"];

static STATUS_REQUEST_V1: Lazy<Vec<u8>> =
    Lazy::new(|| rmp_serde::to_vec_named(&LegacyProviderRequest::Status).unwrap());
static STATUS_REQUEST_V2: Lazy<Vec<u8>> = Lazy::new(|| {
    rmp_serde::to_vec_named(&ProviderRequest {
        query_type: STATUS_QUERY_TYPE.to_string(),
        query_data: Blob {
            data: Vec::new().into(),
            mime_type: STATUS_MIME_TYPE.to_string(),
        },
        config: Value::Null,
        previous_response: None,
    })
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
                            .map(|data_source| UpsertProxyDataSource {
                                name: data_source.name.clone(),
                                description: data_source.description.clone(),
                                provider_type: data_source.provider_type.clone(),
                                protocol_version: get_protocol_version(&data_source.provider_type),
                                status: DataSourceStatus::Error(Error::ProxyDisconnected),
                            })
                            .collect();
                        let message = ProxyMessage::new_set_data_sources_notification(data_sources);
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
                                    let service = self.clone();
                                    tokio::spawn(async move {
                                        if let Err(err) = service.handle_message(message, outgoing_sender).await {
                                            error!("Error handling message: {:?}", err);
                                        };
                                    }.in_current_span());
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

        debug!(%protocol_version, %data_source.provider_type, "Invoking provider");
        let response = match (message.protocol_version, message.payload) {
            (1, ServerMessagePayload::Invoke(message)) => {
                self.handle_invoke_proxy_message_v1(message, runtime, &data_source, op_id)
                    .await
            }
            (2, ServerMessagePayload::Invoke(message)) => {
                self.handle_invoke_proxy_message_v2(message, runtime, &data_source, op_id)
                    .await
            }
            (2, ServerMessagePayload::CreateCells(message)) => {
                self.handle_create_cells_proxy_message(message, runtime, &data_source, op_id)
            }
            (2, ServerMessagePayload::ExtractData(message)) => {
                self.handle_extract_data_proxy_message(message, runtime, &data_source, op_id)
            }
            (2, ServerMessagePayload::GetConfigSchema(message)) => {
                self.handle_config_schema_proxy_message(message, runtime, &data_source, op_id)
            }
            (2, ServerMessagePayload::GetSupportedQueryTypes(message)) => {
                self.handle_supported_query_types(message, runtime, &data_source, op_id)
                    .await
            }
            (_, payload) => ProxyMessage::new_error_response(
                Error::Invocation {
                    message: format!(
                        "unsupported (protocol version, payload) pair: ({}, {:?})",
                        message.protocol_version, payload
                    ),
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
    async fn handle_invoke_proxy_message_v1(
        &self,
        message: InvokeRequest,
        runtime: &Runtime,
        data_source: &ProxyDataSource,
        op_id: Base64Uuid,
    ) -> ProxyMessage {
        let result =
            bindings::invoke_provider_v1(runtime, message.data, data_source.config.clone()).await;
        match result {
            Ok(response) => ProxyMessage::new_invoke_proxy_response(response, op_id),
            Err(error) => ProxyMessage::new_error_response(error, op_id),
        }
    }

    #[instrument(skip_all, fields(
        trace_id = ?op_id,
        data_source_name = ?data_source.name,
    ))]
    async fn handle_invoke_proxy_message_v2(
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
                let response = if V1_PROVIDERS.contains(&data_source.provider_type.as_str()) {
                    self.check_provider_status_v1(name.clone()).await
                } else {
                    self.check_provider_status_v2(name.clone()).await
                };

                let status = match response {
                    Ok(_) => DataSourceStatus::Connected,
                    Err(ref err) => DataSourceStatus::Error(err.clone()),
                };

                if let Some((delay, task)) = task.next() {
                    if response.is_err() {
                        warn!(
                            "error connecting to data source: {name}, retrying in {}s",
                            delay.as_secs()
                        );
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            individual_check_task_queue_tx.send(task)
                        });
                    }
                }

                UpsertProxyDataSource {
                    name: name.clone(),
                    description: data_source.description.clone(),
                    provider_type: data_source.provider_type.clone(),
                    protocol_version: get_protocol_version(&data_source.provider_type),
                    status,
                }
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

    #[instrument(err, skip(self))]
    async fn check_provider_status_v1(&self, data_source_name: Name) -> Result<(), Error> {
        debug!(
            "Using protocol v1 to check provider status: {}",
            &data_source_name
        );
        let message = ServerMessage::new_invoke_proxy_request(
            STATUS_REQUEST_V1.clone(),
            data_source_name,
            1,
            Base64Uuid::new(),
        );
        let response = self
            .handle_message_inner(message)
            .await
            .expect("The handler only fails if the message has no op_id.");
        let response = match response.payload {
            ProxyMessagePayload::InvokeProxyResponse(response) => response,
            ProxyMessagePayload::Error(err) => {
                return Err(err.error);
            }
            _ => {
                return Err(Error::Other {
                    message: format!("Unexpected response from provider: {response:?}"),
                });
            }
        };
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
            }) => Err(Error::Http {
                error: HttpRequestError::ServerError {
                    status_code,
                    response,
                },
            }),
            Ok(LegacyProviderResponse::Error { error }) => Err(error),
            Err(err) => Err(Error::Deserialization {
                message: format!("Error deserializing provider response: {err:?}"),
            }),
            _ => Err(Error::Other {
                message: format!("Unexpected provider response: {response:?}"),
            }),
        }
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
fn get_protocol_version(provider_type: &str) -> u8 {
    if V1_PROVIDERS.contains(&provider_type) {
        1
    } else {
        2
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
                        (&Method::GET, "/metrics") => match metrics_export() {
                            Ok(metrics) => (StatusCode::OK, Body::from(metrics)),
                            Err(err) => (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Body::from(format!("Error exporting metrics: {err}")),
                            ),
                        },
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
