use fp_provider_runtime::spec::types::{FetchError, Instant, Series, TimeRange, Timestamp};
use rmp_serde::decode::Error as DecodeError;
use rmp_serde::encode::Error as EncodeError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;
use thiserror::Error;
use uuid::Uuid;

/// Messages intended for the Server to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    FetchData(FetchDataMessage),
}

impl ServerMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> Result<ServerMessage, DecodeError> {
        rmp_serde::from_read_ref(&input)
    }

    pub fn serialize_msgpack(&self) -> Result<Vec<u8>, EncodeError> {
        rmp_serde::to_vec(&self)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataMessage {
    pub op_id: Uuid,
    pub data_source_name: String,
    pub query: String,
    pub query_type: QueryType,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum QueryType {
    // From, To
    Series(TimeRange),

    // Time instant
    Instant(Timestamp),
}

/// Messages intended for the Relay to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    SetDataSources(SetDataSourcesMessage),
    FetchDataResult(FetchDataResultMessage),
}

impl RelayMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> Result<RelayMessage, DecodeError> {
        rmp_serde::from_read_ref(&input)
    }

    pub fn serialize_msgpack(&self) -> Result<Vec<u8>, EncodeError> {
        rmp_serde::to_vec(&self)
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
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

#[derive(Error, Debug, PartialEq)]
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataResultMessage {
    pub op_id: Uuid,
    pub result: QueryResult,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum QueryResult {
    Series(Result<Vec<Series>, FetchError>),
    Instant(Result<Vec<Instant>, FetchError>),
}
