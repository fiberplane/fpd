//! Library of fiberplane-provider-protocol binding wrappers

use fiberplane::{
    provider_bindings::host::mem::{deserialize_from_slice, serialize_to_vec},
    provider_bindings::{Blob, Cell, ConfigSchema, Error, ProviderRequest, SupportedQueryType},
    provider_runtime::spec::Runtime,
};
use serde_json::{Map, Value};
use tracing::trace;

mod converters;
use converters::SpecToBinding as _;

pub async fn invoke_provider_v1(
    runtime: &mut Runtime,
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
    runtime: &mut Runtime,
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
    runtime: &mut Runtime,
    query_type: &String,
    response: Blob,
) -> Result<Result<Vec<Cell>, Error>, Error> {
    runtime
        .create_cells(query_type.to_string(), response)
        .map(|res| res.map_err(|inner_err| inner_err.convert()))
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
}

pub fn extract_data(
    runtime: &mut Runtime,
    response: Blob,
    mime_type: &String,
    query: &Option<String>,
) -> Result<Result<Blob, Error>, Error> {
    runtime
        .extract_data(response, mime_type.to_string(), query.clone())
        .map(|res| res.map_err(|inner_err| inner_err.convert()))
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
}

pub fn get_config_schema(runtime: &mut Runtime) -> Result<ConfigSchema, Error> {
    // Using the raw wrapper here to avoid deserialing response to Blob, before re-serializing it to Vec<u8> for the call
    runtime
        .get_config_schema()
        .map(|val| val.convert())
        .map_err(|err| Error::Invocation {
            message: format!("Error invoking provider: {:?}", err),
        })
}

pub async fn get_supported_query_types(
    runtime: &mut Runtime,
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
