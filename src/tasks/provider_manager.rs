//! Tasks to handle provider management

use crate::{cli::BuiltinProvider, runtime::providers_wasm_dir};
use std::path::PathBuf;
use thiserror::Error;
use tracing::info;

pub const ALL_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider::Sentry,
    BuiltinProvider::Loki,
    BuiltinProvider::Elasticsearch,
    BuiltinProvider::Https,
    BuiltinProvider::Prometheus,
];

#[derive(Debug, Error)]
pub enum Error {
    #[error("Runtime error: {0}")]
    Runtime(#[from] crate::runtime::Error),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Provider {provider} not found")]
    NotFound { provider: String },
    #[error("Provider {provider} exists at {}, delete it manually if you want to replace this", path.display())]
    NoOverwrite { provider: String, path: PathBuf },
    #[error(

                        "Errors while fetching providers:\n\t{}",
                        errors
                            .iter()
                            .map(|err| err.to_string())
                            .collect::<Vec<String>>()
                            .join("\n\t")

    )]
    Multiple { errors: Vec<Error> },
}

pub async fn pull(providers: &[BuiltinProvider], all: bool) -> Result<(), Error> {
    let wasm_dir = providers_wasm_dir()?;
    if wasm_dir.exists() && !wasm_dir.is_dir() {
        return Err(Error::Runtime(
            crate::runtime::Error::ProvidersDirUnavailable(wasm_dir),
        ));
    }

    if !wasm_dir.exists() {
        std::fs::create_dir_all(wasm_dir)?;
    }

    let providers: &[BuiltinProvider] = if all || providers.is_empty() {
        ALL_PROVIDERS
    } else {
        providers
    };
    let mut errors = Vec::new();
    for provider in providers {
        if let Err(err) = fetch_provider(*provider).await {
            errors.push(err)
        }
    }
    if !errors.is_empty() {
        return Err(Error::Multiple { errors });
    }
    Ok(())
}

async fn fetch_provider(provider: BuiltinProvider) -> Result<(), Error> {
    let wasm_dir = providers_wasm_dir()?;
    let provider_name = provider.name();
    let location_url =
        format!("https://github.com/fiberplane/proxy/raw/main/providers/{provider_name}.wasm");

    info!("Fetching {provider_name} provider at {location_url}");

    let bytes = reqwest::get(location_url).await?.bytes().await?;

    let target_path = wasm_dir.join(format!("{provider_name}.wasm"));
    if target_path.exists() {
        return Err(Error::NoOverwrite {
            provider: provider_name,
            path: target_path,
        });
    }

    std::fs::write(target_path, bytes)?;
    Ok(())
}
