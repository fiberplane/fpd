use rmp_serde::decode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Display};
use std::str::FromStr;
pub use uuid::Uuid;

/// Messages intended for the Server to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    InvokeProxy(InvokeProxyMessage),
}

impl ServerMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> Result<ServerMessage, decode::Error> {
        rmp_serde::from_read_ref(&input)
    }

    pub fn serialize_msgpack(&self) -> Vec<u8> {
        rmp_serde::to_vec(&self).expect("MessgePack serialization error")
    }

    pub fn op_id(&self) -> Option<Uuid> {
        match self {
            ServerMessage::InvokeProxy(message) => Some(message.op_id),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeProxyMessage {
    pub op_id: Uuid,
    pub data_source_name: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl Debug for InvokeProxyMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvokeProxyMessage")
            .field("op_id", &self.op_id)
            .field("data_source_name", &self.data_source_name)
            .field("data", &format!("[{} bytes]", self.data.len()))
            .finish()
    }
}

/// Messages intended for the Relay to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RelayMessage {
    SetDataSources(SetDataSourcesMessage),
    InvokeProxyResponse(InvokeProxyResponseMessage),
    Error(ErrorMessage),
}

impl From<ErrorMessage> for RelayMessage {
    fn from(message: ErrorMessage) -> Self {
        RelayMessage::Error(message)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeProxyResponseMessage {
    pub op_id: Uuid,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

impl Debug for InvokeProxyResponseMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InvokeProxyResponseMessage")
            .field("op_id", &self.op_id)
            .field("data", &format!("[{} bytes]", self.data.len()))
            .finish()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorMessage {
    pub op_id: Uuid,
    pub message: String,
}

impl ErrorMessage {
    pub fn new(op_id: Uuid, message: impl Into<String>) -> Self {
        Self {
            op_id,
            message: message.into(),
        }
    }
}

impl RelayMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> Result<RelayMessage, decode::Error> {
        rmp_serde::from_read_ref(&input)
    }

    pub fn serialize_msgpack(&self) -> Vec<u8> {
        rmp_serde::to_vec(&self).expect("MessgePack serialization error")
    }

    pub fn op_id(&self) -> Option<Uuid> {
        match self {
            RelayMessage::InvokeProxyResponse(message) => Some(message.op_id),
            RelayMessage::Error(error) => Some(error.op_id),
            RelayMessage::SetDataSources(_) => None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DataSourceType {
    Prometheus,
    Elasticsearch,
    Loki,
}

impl Display for DataSourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DataSourceType::Prometheus => "prometheus",
            DataSourceType::Elasticsearch => "elasticsearch",
            DataSourceType::Loki => "loki",
        };
        f.write_str(s)
    }
}

#[derive(thiserror::Error, Debug, PartialEq)]
#[error("Unexpected data source type: {0}")]
pub struct UnexpectedDataSourceType(String);

impl FromStr for DataSourceType {
    type Err = UnexpectedDataSourceType;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "prometheus" => Ok(DataSourceType::Prometheus),
            "elasticsearch" => Ok(DataSourceType::Elasticsearch),
            "loki" => Ok(DataSourceType::Loki),
            _ => Err(UnexpectedDataSourceType(s.to_string())),
        }
    }
}

/// This is a map from the data source name to the data source's type
pub type SetDataSourcesMessage = HashMap<String, DataSourceType>;

#[test]
fn data_source_type_should_serialize_to_plain_string() {
    let ty = DataSourceType::Prometheus;
    let serialized = serde_json::to_string(&ty).unwrap();
    assert_eq!(serialized, "\"prometheus\"");
}
