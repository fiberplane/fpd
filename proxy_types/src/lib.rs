pub use fp_provider_runtime::spec::types::{Error, Instant, Series, TimeRange, Timestamp};
use rmp_serde::decode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;
pub use uuid::Uuid;

/// Messages intended for the Server to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    // TODO should we have a more specific name?
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeProxyMessage {
    pub op_id: Uuid,
    pub data_source_name: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Messages intended for the Relay to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum RelayMessage {
    SetDataSources(SetDataSourcesMessage),
    InvokeProxyResponse(InvokeProxyResponseMessage),
    Error(ErrorMessage),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvokeProxyResponseMessage {
    pub op_id: Uuid,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorMessage {
    pub op_id: Uuid,
    pub message: String,
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
#[serde(tag = "type", rename_all = "camelCase")]
pub enum DataSourceType {
    Prometheus,
}

impl Display for DataSourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DataSourceType::Prometheus => "prometheus",
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
            _ => Err(UnexpectedDataSourceType(s.to_string())),
        }
    }
}

/// This is a map from the data source name to the data source's type
pub type SetDataSourcesMessage = HashMap<String, DataSourceType>;
