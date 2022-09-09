use fiberplane::protocols::data_sources::DataSource;
use fiberplane::protocols::names::Name;
use rmp_serde::decode;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
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
    pub data_source_name: Name,
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

/// This is a map from the data source name to the data source's type
pub type SetDataSourcesMessage = Vec<DataSource>;
