use crate::data_sources::DataSources;
use anyhow::{anyhow, Context, Error, Result};
use fp_provider_runtime::spec::types::{QueryInstantOptions, QuerySeriesOptions};
use fp_provider_runtime::Runtime;
use futures::select;
use futures::{sink::SinkExt, FutureExt, StreamExt};
use hyper_tungstenite::tungstenite::Message;
use proxy_types::{
    FetchDataMessage, FetchDataResultMessage, QueryResult, QueryType, RelayMessage, ServerMessage,
    SetDataSourcesMessage,
};
use rmp_serde::Serializer;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::connect_async;
use tracing::{debug, error, trace};
use url::Url;
use wasmer::{Singlepass, Store, Universal};

const WS_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(45);

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
}

impl ProxyService {
    /// Load the provider wasm files from the given directory and create a new Proxy instance
    pub async fn init(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_dir: &Path,
        data_sources: DataSources,
    ) -> Result<Self> {
        let wasm_modules = load_wasm_modules(wasm_dir, &data_sources).await?;
        Ok(ProxyService::new(
            fiberplane_endpoint,
            auth_token,
            wasm_modules,
            data_sources,
        ))
    }

    pub(crate) fn new(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_modules: WasmModuleMap,
        data_sources: DataSources,
    ) -> Self {
        ProxyService {
            inner: Arc::new(Inner {
                endpoint: fiberplane_endpoint,
                auth_token,
                wasm_modules,
                data_sources,
            }),
        }
    }

    pub async fn connect(&self) -> Result<()> {
        // open ws connection
        let request = http::Request::builder()
            .uri(self.inner.endpoint.as_str())
            .header("fp-auth-token", self.inner.auth_token.clone())
            .body(())?;

        let (ws_stream, resp) = connect_async(request).await?;

        let connection_id = resp
            .headers()
            .get("x-fp-conn-id")
            .and_then(|id| id.to_str().ok());
        if let Some(connection_id) = connection_id {
            debug!("connection established, connection id: {}", connection_id);
        } else {
            debug!("connection established, no connection id provided");
        }

        let (mut write, mut read) = ws_stream.split();

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
        write.send(message).await?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<RelayMessage>();

        // We use a local task set because the Wasmer runtime embedded in the ProxyService
        // cannot be moved across threads (which would be necessary to spawn a task that
        // includes the service)
        let local = tokio::task::LocalSet::new();

        let service = self.clone();
        let read_handle = local.run_until(async move {
            tokio::task::spawn_local(async move {
                use hyper_tungstenite::tungstenite::Message::*;
                while let Some(message) = read.next().await {
                    let message =
                        message.with_context(|| "Error reading next websocket message")?;
                    match message {
                        Text(_) => error!("Received Text"),
                        Binary(msg) => {
                            service
                                .handle_message(
                                    ServerMessage::deserialize_msgpack(msg).with_context(|| {
                                        "Error deserializing server message as msgpack"
                                    })?,
                                    tx.clone(),
                                )
                                .await?
                        }
                        Ping(_) => trace!("Received Ping"),
                        Pong(_) => trace!("Received Pong"),
                        Close(_) => {
                            debug!("Received Close");
                            break;
                        }
                    }
                }

                Ok::<_, Error>(read)
            })
            .await?
        });

        let write_handle = tokio::spawn(async move {
            trace!("handle_command: creating write_handle");

            loop {
                select! {
                    message = rx.recv().fuse() => {
                        match message {
                            Some(message) => {
                                trace!("handle_command: sending message to relay");

                                let mut buf = Vec::new();
                                message.serialize(&mut Serializer::new(&mut buf))
                                    .with_context(|| "Error serializing RelayMessage to binary")?;

                                write.send(Message::Binary(buf)).await?;

                                trace!("handle_command: sending message to relay complete");
                            },
                            None => { break;}
                        }
                    }
                    _ = sleep(WS_INACTIVITY_TIMEOUT).fuse() => {
                        write.send(Message::Ping(b"ping".to_vec())).await?;
                    }
                }
            }

            Ok::<_, Error>(write)
        });

        // keep connection open and handle incoming connections
        let (read, write) = futures::join!(read_handle, write_handle);

        trace!("handle_command: reuniting read and write, and closing them");
        // TODO is there a way to get rid of the double question mark?
        let websocket = read?.reunite(write??);

        trace!("closing connection");
        websocket?.close(None).await?;

        trace!("connection closed");

        Ok(())
    }

    async fn handle_message(
        &self,
        message: ServerMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        match message {
            ServerMessage::FetchData(message) => {
                self.handle_relay_query_message(message, reply).await
            }
        }
    }

    async fn handle_relay_query_message(
        &self,
        message: FetchDataMessage,
        reply: UnboundedSender<RelayMessage>,
    ) -> Result<()> {
        let data_source_name = message.data_source_name.as_str();
        debug!(
            "received a relay message for data source {}: {:?}",
            data_source_name, message
        );
        let runtime = self.create_runtime(data_source_name).await?;

        let query = message.query;
        let data_source = self
            .inner
            .data_sources
            .get(data_source_name)
            // TODO send error message back to caller
            .ok_or_else(|| anyhow!(format!("unknown data source: {}", data_source_name)))?
            .clone()
            // convert to the fp_provider_runtime type
            .into();

        // Execute either a series or an instant query
        let query_result = match message.query_type {
            QueryType::Series(time_range) => {
                let options = QuerySeriesOptions {
                    data_source,
                    time_range,
                };
                let result = runtime
                    .fetch_series(query, options)
                    .await
                    .with_context(|| "Wasmer runtime error while running fetch_series query")?;
                QueryResult::Series(result)
            }
            QueryType::Instant(time) => {
                let options = QueryInstantOptions { data_source, time };
                let result = runtime
                    .fetch_instant(query, options)
                    .await
                    .with_context(|| "Wasmer runtime error while running fetch_instant query")?;
                QueryResult::Instant(result)
            }
        };

        // TODO: Better handling of invocation errors. Do we send something back in
        // that case, and/or log to stderr?

        let fetch_data_result_message = FetchDataResultMessage {
            op_id: message.op_id,
            result: query_result,
        };

        reply.send(RelayMessage::FetchDataResult(fetch_data_result_message))?;

        Ok(())
    }

    async fn create_runtime(&self, data_source_name: &str) -> Result<Runtime> {
        let data_source_type = match self.inner.data_sources.get(data_source_name) {
            Some(data_source) => data_source.ty(),
            None => {
                return Err(anyhow!(
                    "received relay message for unknown data source: {}",
                    data_source_name
                ))
            }
        };
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
