use crate::common::{
    FetchDataMessage, FetchDataResultMessage, QueryResult, QueryType, RelayMessage, ServerMessage,
    SetDataSourcesMessage,
};
use crate::data_sources::DataSources;
use anyhow::{anyhow, Error, Result};
use fp_provider_runtime::spec::types::{QueryInstantOptions, QuerySeriesOptions};
use fp_provider_runtime::Runtime;
use futures::{sink::SinkExt, StreamExt};
use hyper_tungstenite::tungstenite::Message;
use rmp_serde::Serializer;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::connect_async;
use tracing::{debug, error, trace};
use url::Url;
use wasmer::{Singlepass, Store, Universal};

#[derive(Clone, Debug)]
pub struct ProxyService {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    endpoint: Url,
    auth_token: String,
    wasm_dir: PathBuf,
    data_sources: DataSources,
}

impl ProxyService {
    pub fn new(
        fiberplane_endpoint: Url,
        auth_token: String,
        wasm_dir: PathBuf,
        data_sources: DataSources,
    ) -> Self {
        ProxyService {
            inner: Arc::new(Inner {
                endpoint: fiberplane_endpoint,
                auth_token,
                wasm_dir,
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

        let connection_id = resp.headers().get("x-fp-conn-id");
        match connection_id {
            Some(val) => debug!(
                "connection established, connection id: {}",
                val.to_str().unwrap()
            ),
            None => debug!("connection established, no connection id provided"),
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
        write
            .send(Message::Binary(message.serialize_msgpack()))
            .await?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();

        let local = tokio::task::LocalSet::new();

        let service = self.clone();
        let read_handle = local.run_until(async move {
            tokio::task::spawn_local(async move {
                use hyper_tungstenite::tungstenite::Message::*;
                while let Some(message) = read.next().await {
                    match message.unwrap() {
                        Text(_) => error!("Received Text"),
                        Binary(msg) => {
                            service
                                .handle_message(ServerMessage::deserialize_msgpack(msg), tx.clone())
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

            while let Some(message) = rx.recv().await {
                write.send(message).await?;
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
        reply: UnboundedSender<Message>,
    ) -> Result<()> {
        let response = match message {
            ServerMessage::FetchData(message) => self.handle_relay_query_message(message).await?,
        };

        trace!("handle_command: sending message to relay");
        let mut buf = Vec::new();
        response.serialize(&mut Serializer::new(&mut buf)).unwrap();
        reply.send(Message::Binary(buf))?;
        trace!("handle_command: finished sending message to relay");

        Ok(())
    }

    async fn handle_relay_query_message(&self, message: FetchDataMessage) -> Result<RelayMessage> {
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
                let result = runtime.fetch_series(query, options).await;
                QueryResult::Series(result.unwrap())
            }
            QueryType::Instant(time) => {
                let options = QueryInstantOptions { data_source, time };
                let result = runtime.fetch_instant(query, options).await;
                QueryResult::Instant(result.unwrap())
            }
        };

        // TODO: Better handling of invocation errors. Do we send something back in
        // that case, and/or log to stderr?

        let fetch_data_result_message = FetchDataResultMessage {
            op_id: message.op_id,
            result: query_result,
        };

        Ok(RelayMessage::FetchDataResult(fetch_data_result_message))
    }

    async fn create_runtime(&self, data_source_name: &str) -> Result<Runtime> {
        // WASM files are stored in the WASM directory as providerName.wasm
        // (for example, /path/to/wasm/dir/prometheus.wasm)
        let wasm_path = &self
            .inner
            .wasm_dir
            .with_file_name(data_source_name)
            .with_extension("wasm");
        // TODO: Preload and/or cache the result
        let wasm_module = fs::read(wasm_path).await?;

        let engine = Universal::new(Singlepass::default()).engine();
        let store = Store::new(&engine);

        let runtime = Runtime::new(store, wasm_module)?;
        Ok(runtime)
    }
}
