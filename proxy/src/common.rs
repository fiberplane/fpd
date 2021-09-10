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
    pub query: String,
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
    pub result: Result<String, String>,
}
