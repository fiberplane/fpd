use fiberplane::protocols::data_sources::DataSourceStatus;
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
    pub protocol_version: u8,
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
        rmp_serde::to_vec_named(&self).expect("MessgePack serialization error")
    }

    pub fn op_id(&self) -> Option<Uuid> {
        match self {
            RelayMessage::InvokeProxyResponse(message) => Some(message.op_id),
            RelayMessage::Error(error) => Some(error.op_id),
            RelayMessage::SetDataSources(_) => None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDataSourcesMessage {
    pub data_sources: Vec<UpsertProxyDataSource>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub struct UpsertProxyDataSource {
    pub name: Name,
    pub description: Option<String>,
    pub provider_type: String,
    #[serde(flatten)]
    pub status: DataSourceStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use fiberplane::protocols::providers::Error;

    #[test]
    fn serialization_deserialization() {
        let data_sources = vec![
            UpsertProxyDataSource {
                name: Name::from_static("prometheus-prod"),
                provider_type: "prometheus".to_string(),
                description: Some("Production Prometheus".to_string()),
                status: DataSourceStatus::Connected,
            },
            UpsertProxyDataSource {
                name: Name::from_static("elasticsearch-prod"),
                provider_type: "elasticsearch".to_string(),
                description: None,
                status: DataSourceStatus::Error(Error::NotFound),
            },
        ];
        let message = RelayMessage::SetDataSources(SetDataSourcesMessage {
            data_sources: data_sources.clone(),
        });
        let serialized = message.serialize_msgpack();
        let deserialized = RelayMessage::deserialize_msgpack(serialized).unwrap();
        if let RelayMessage::SetDataSources(set_data_sources) = deserialized {
            assert_eq!(set_data_sources.data_sources, data_sources)
        } else {
            panic!("Unexpected message type");
        }
    }
}
