//! Library of fiberplane-provider-protocol binding wrappers

use fiberplane::{
    models::providers::ConfigSchema,
    provider_bindings::{
        host::mem::{deserialize_from_slice, serialize_to_vec},
        Cell, Error, ProviderRequest, SupportedQueryType,
    },
    provider_runtime::spec::Runtime,
};
use serde_json::{Map, Value};
use tracing::trace;

pub async fn invoke_provider_v1(
    runtime: &Runtime,
    request: Vec<u8>,
    config: Map<String, Value>,
) -> Result<Vec<u8>, Error> {
    let config = rmp_serde::to_vec_named(&config).map_err(|err| Error::Config {
        message: format!("Error serializing config as JSON: {:?}", err),
    })?;
    runtime
        .invoke_raw(request, config)
        .await
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
}

pub async fn invoke_provider_v2(
    runtime: &Runtime,
    request: Vec<u8>,
    config: Map<String, Value>,
) -> Result<Vec<u8>, Error> {
    // In v2, the request is a single object so we need to deserialize it to inject the config
    let mut request: ProviderRequest =
        rmp_serde::from_slice(&request).map_err(|err| Error::Deserialization {
            message: format!("Error deserializing provider request: {:?}", err),
        })?;
    trace!("Provider request: {:?}", request);
    request.config = Value::Object(config);
    let request = rmp_serde::to_vec_named(&request).map_err(|err| Error::Deserialization {
        message: format!("Error serializing request: {:?}", err),
    })?;
    runtime
        .invoke2_raw(request)
        .await
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
}

pub fn create_cells(
    runtime: &Runtime,
    query_type: &String,
    response: Vec<u8>,
) -> Result<Vec<Cell>, Error> {
    // Using the raw wrapper here to avoid deserialing response to Blob, before re-serializing it to Vec<u8> for the call
    runtime
        .create_cells_raw(serialize_to_vec(query_type), response)
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
        .and_then(|ref result| deserialize_from_slice(result))
}

pub fn extract_data(
    runtime: &Runtime,
    response: Vec<u8>,
    mime_type: &String,
    query: &Option<String>,
) -> Result<Vec<u8>, Error> {
    // Using the raw wrapper here to avoid deserialing response to Blob, before re-serializing it to Vec<u8> for the call
    runtime
        .extract_data_raw(
            response,
            serialize_to_vec(mime_type),
            serialize_to_vec(query),
        )
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
}

pub fn get_config_schema(runtime: &Runtime) -> Result<ConfigSchema, Error> {
    // Using the raw wrapper here to avoid deserialing response to Blob, before re-serializing it to Vec<u8> for the call
    runtime
        .get_config_schema_raw()
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
        .and_then(|ref result| deserialize_from_slice(result))
}

pub async fn get_supported_query_types(
    runtime: &Runtime,
    config: &Map<String, Value>,
) -> Result<Vec<SupportedQueryType>, Error> {
    let config = Value::Object(config.clone());
    // Using the raw wrapper here to avoid deserialing response to Blob, before re-serializing it to Vec<u8> for the call
    runtime
        .get_supported_query_types_raw(serialize_to_vec(&config))
        .await
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
        .and_then(|ref result| deserialize_from_slice(result))
}
