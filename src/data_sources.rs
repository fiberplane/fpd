use crate::common::DataSourceType;
use fp_provider_runtime::spec::types::{DataSource as RuntimeDataSource, PrometheusDataSource};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::From;

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "camelCase", tag = "type", content = "options")]
// Note this is the same as the DataSource enum exported by the fp_provider_runtime
// with the exception that here we use serde's adjacently typed enum serialization
// whereas the fp_provider_runtime uses the internally tagged representation.
// By using the adjecently tagged version in the config file, it allows us to add
// additional fields to DataSources without worrying about conflicts with the
// parameters defined by individual data sources.
pub enum DataSource {
    Prometheus(PrometheusDataSource),
}

impl From<DataSource> for RuntimeDataSource {
    fn from(d: DataSource) -> Self {
        match d {
            DataSource::Prometheus(options) => RuntimeDataSource::Prometheus(options),
        }
    }
}

impl From<&DataSource> for DataSourceType {
    fn from(d: &DataSource) -> Self {
        match d {
            DataSource::Prometheus(_) => DataSourceType::Prometheus,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
// note this uses camel case to be consistent with the fp_provider_runtime types
pub struct DataSources(pub HashMap<String, DataSource>);
