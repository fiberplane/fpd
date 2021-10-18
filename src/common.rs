use fp_provider_runtime::spec::types::{FetchError, Instant, Series, TimeRange, Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Messages intended for the Server to handle
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    FetchData(FetchDataMessage),
}

impl ServerMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> ServerMessage {
        rmp_serde::from_read_ref(&input).unwrap()
    }

    pub fn serialize_msgpack(&self) -> Vec<u8> {
        rmp_serde::to_vec(&self).unwrap()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataMessage {
    pub op_id: uuid::Uuid,
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
    pub fn deserialize_msgpack(input: Vec<u8>) -> RelayMessage {
        rmp_serde::from_read_ref(&input).unwrap()
    }

    pub fn serialize_msgpack(&self) -> Vec<u8> {
        rmp_serde::to_vec(&self).unwrap()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DataSourceType {
    Prometheus,
}

/// This is a map from the data source name to the data source's type
pub type SetDataSourcesMessage = HashMap<String, DataSourceType>;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchDataResultMessage {
    pub op_id: uuid::Uuid,
    pub result: QueryResult,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum QueryResult {
    Series(Result<Vec<Series>, FetchError>),
    Instant(Result<Vec<Instant>, FetchError>),
}
