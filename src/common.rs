use fp_provider_runtime::spec::types::{FetchError, Instant, Series};
use serde::{Deserialize, Serialize};

/// Messages intended for the Server to handle
#[derive(Debug, Deserialize, Serialize)]
pub enum ServerMessage {
    FetchData(FetchDataMessage),
}

impl ServerMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> ServerMessage {
        rmp_serde::from_read_ref(&input).unwrap()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FetchDataMessage {
    pub op_id: uuid::Uuid,
    pub data_source_name: String,
    pub query: String,
    pub query_type: QueryType,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum QueryType {
    // From, To
    Series(f64, f64),

    // Time instant
    Instant(f64),
}

/// Messages intended for the Relay to handle
#[derive(Debug, Deserialize, Serialize)]
pub enum RelayMessage {
    FetchDataResult(FetchDataResultMessage),
}

impl RelayMessage {
    pub fn deserialize_msgpack(input: Vec<u8>) -> RelayMessage {
        rmp_serde::from_read_ref(&input).unwrap()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FetchDataResultMessage {
    pub op_id: uuid::Uuid,
    pub result: QueryResult,
}

#[derive(Debug, Deserialize, Serialize)]
pub enum QueryResult {
    Series(Result<Vec<Series>, FetchError>),
    Instant(Result<Vec<Instant>, FetchError>),
}
