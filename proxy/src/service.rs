use anyhow::{anyhow, Context, Result};
use fiberplane::protocols::providers::{Error, STATUS_MIME_TYPE, STATUS_QUERY_TYPE};
use fiberplane::protocols::{data_sources::DataSourceStatus, names::Name, proxies::ProxyToken};
use fp_provider_bindings::{Blob, HttpRequestError, LegacyProviderRequest, LegacyProviderResponse};
use fp_provider_runtime::spec::{types::ProviderRequest, Runtime};
use futures::{future::join_all, select, FutureExt};
use http::{Method, Request, Response, StatusCode};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Server};
use once_cell::sync::Lazy;
use proxy_types::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::{convert::Infallible, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::{broadcast::Sender, watch};
use tokio::{fs, time::interval};
use tokio_tungstenite_reconnect::{Message, ReconnectingWebSocket};
use tracing::{debug, error, info, info_span, instrument, trace, Instrument, Span};
use url::Url;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProxyDataSource {
    pub name: Name,
    pub provider_type: String,
    pub config: Map<String, Value>,
    pub description: Option<String>,
}

pub(crate) type WasmModules = HashMap<String, Result<Runtime, String>>;
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
                wasm_modules,
                max_retries,
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
                        let data_sources = service
                            .inner
                            .data_sources
                            .values()
                            .map(|data_source| UpsertProxyDataSource {
                                name: data_source.name.clone(),
                                description: data_source.description.clone(),
                                provider_type: data_source.provider_type.clone(),
                                status: DataSourceStatus::Error(Error::ProxyDisconnected),
                            })
                            .collect();
                        let message = RelayMessage::SetDataSources(SetDataSourcesMessage{ data_sources});
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
        let response = match message {
            ServerMessage::InvokeProxy(message) => self.handle_invoke_proxy_message(message).await,
        };

        reply
            .send(response)
            .context("Error sending response to relay")?;

        Ok(())
    }

    #[instrument(skip_all, fields(
        trace_id = ?message.op_id,
        data_source_name = ?message.data_source_name,
        message.data = %msgpack_to_json(&message.data).unwrap_or_default()
    ))]
    async fn handle_invoke_proxy_message(&self, message: InvokeProxyMessage) -> RelayMessage {
        debug!("handling relay message");
        let op_id = message.op_id;

        // Try to create the runtime for the given data source
        let data_source = match self.inner.data_sources.get(&message.data_source_name) {
            Some(data_source) => data_source.clone(),
            None => {
                error!("received relay message for unknown data source");
                return RelayMessage::Error(ErrorMessage {
                    op_id,
                    message: format!(
                        "received relay message for unknown data source: {}",
                        message.data_source_name
                    ),
                });
            }
        };

        let runtime: Runtime = match &self.inner.wasm_modules[&data_source.provider_type] {
            Ok(runtime) => runtime.clone(),
            Err(message) => {
                return RelayMessage::Error(ErrorMessage {
                    op_id,
                    message: message.to_string(),
                });
            }
        };

        let result = match message.protocol_version {
            1 => invoke_provider_v1(runtime, message.data, data_source.config.clone()).await,
            2 => invoke_provider_v2(runtime, message.data, data_source.config.clone()).await,
            _ => Err(format!(
                "unsupported protocol version: {}",
                message.protocol_version
            )),
        };
        match result {
            Ok(response) => RelayMessage::InvokeProxyResponse(InvokeProxyResponseMessage {
                op_id,
                data: response,
            }),
            Err(message) => RelayMessage::Error(ErrorMessage { op_id, message }),
        }
    }

    async fn get_data_sources(&self) -> SetDataSourcesMessage {
        let data_sources = join_all(self.inner.data_sources.iter().map(
            |(name, data_source)| async move {
                let response = if V1_PROVIDERS.contains(&data_source.provider_type.as_str()) {
                    self.check_provider_status_v1(name.clone()).await
                } else {
                    self.check_provider_status_v2(name.clone()).await
                };

                let status = match response {
                    Ok(_) => DataSourceStatus::Connected,
                    Err(err) => DataSourceStatus::Error(err),
                };

                UpsertProxyDataSource {
                    name: name.clone(),
                    description: data_source.description.clone(),
                    provider_type: data_source.provider_type.clone(),
                    status,
                }
            },
        ))
        .await
        .into_iter()
        .collect();
        SetDataSourcesMessage { data_sources }
    }

    #[instrument(err, skip(self))]
    async fn check_provider_status_v1(&self, data_source_name: Name) -> Result<(), Error> {
        debug!(
            "Using protocol v1 to check provider status: {}",
            &data_source_name
        );
        let message = InvokeProxyMessage {
            op_id: Uuid::new_v4(),
            data_source_name,
            data: STATUS_REQUEST_V1.clone(),
            protocol_version: 1,
        };
        let response = self.handle_invoke_proxy_message(message).await;
        let response = match response {
            RelayMessage::InvokeProxyResponse(response) => response,
            RelayMessage::Error(err) => {
                return Err(Error::Invocation {
                    message: err.message,
                });
            }
            _ => {
                return Err(Error::Other {
                    message: format!("Unexpected response from provider: {:?}", response),
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
                message: format!("Error deserializing provider response: {:?}", err),
            }),
            _ => Err(Error::Other {
                message: format!("Unexpected provider response: {:?}", response),
            }),
        }
    }

    #[instrument(skip(self))]
    async fn check_provider_status_v2(&self, data_source_name: Name) -> Result<(), Error> {
        debug!(
            "Using protocol v2 to check provider status: {}",
            &data_source_name
        );
        let message = InvokeProxyMessage {
            op_id: Uuid::new_v4(),
            data_source_name: data_source_name.clone(),
            data: STATUS_REQUEST_V2.clone(),
            protocol_version: 2,
        };

        match self.handle_invoke_proxy_message(message).await {
            RelayMessage::InvokeProxyResponse(response) => {
                let result: Result<Blob, Error> =
                    rmp_serde::from_slice(&response.data).map_err(|err| {
                        Error::Deserialization {
                            message: format!("Error deserializing provider response: {}", err),
                        }
                    })?;
                result?;
                Ok(())
            }
            RelayMessage::Error(error) => Err(Error::Invocation {
                message: format!(
                    "Error invoking provider for data source: {} {}",
                    data_source_name, error.message
                ),
            }),
            message => Err(Error::Invocation {
                message: format!("Unexpected provider response: {:?}", message),
            }),
        }
    }
}

async fn load_wasm_modules(wasm_dir: &Path, provider_types: Vec<String>) -> WasmModules {
    let runtimes = join_all(provider_types.iter().map(|data_source_type| async move {
        // Each provider's wasm module is found in the wasm_dir as data_source_type.wasm
        let wasm_path = &wasm_dir.join(&format!("{}.wasm", &data_source_type));
        let wasm_module = fs::read(wasm_path)
            .await
            .map_err(|err| format!("Error reading wasm file: {}", err))?;
        Runtime::new(wasm_module).map_err(|err| format!("Error compiling wasm module: {}", err))
    }))
    .await;

    provider_types.into_iter().zip(runtimes).collect()
}

async fn invoke_provider_v1(
    runtime: Runtime,
    request: Vec<u8>,
    config: Map<String, Value>,
) -> Result<Vec<u8>, String> {
    let config = rmp_serde::to_vec_named(&config)
        .map_err(|err| format!("Error serializing config as JSON: {:?}", err))?;
    runtime
        .invoke_raw(request, config)
        .await
        .map_err(|err| format!("Error invoking provider: {:?}", err))
}

async fn invoke_provider_v2(
    runtime: Runtime,
    request: Vec<u8>,
    config: Map<String, Value>,
) -> Result<Vec<u8>, String> {
    // In v2, the request is a single object so we need to deserialize it to inject the config
    let mut request: ProviderRequest = rmp_serde::from_slice(&request)
        .map_err(|err| format!("Error deserializing provider request: {:?}", err))?;
    request.config = Value::Object(config);
    let request = rmp_serde::to_vec_named(&request)
        .map_err(|err| format!("Error serializing request: {:?}", err))?;
    runtime
        .invoke2_raw(request)
        .await
        .map_err(|err| format!("Error invoking provider: {:?}", err))
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
