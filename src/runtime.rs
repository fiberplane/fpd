//! Utility functions that allow runtime configuration behaviour

use directories::ProjectDirs;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("No Project Directory is found on this OS.")]
    ProjDir,
    #[error("The expected Providers directory {0} exists but is not a directory.")]
    ProvidersDirUnavailable(PathBuf),
}

pub const QUALIFIER: &str = "dev";
pub const ORGANIZATION_NAME: &str = "fiberplane";
pub const APP_NAME: &str = clap::crate_name!();

/// Return the canonical path to put the sources configuration file
pub fn data_sources_path() -> Result<PathBuf, Error> {
    let proj_dirs =
        ProjectDirs::from(QUALIFIER, ORGANIZATION_NAME, APP_NAME).ok_or(Error::ProjDir)?;
    Ok(proj_dirs.config_dir().join("data_sources.yaml"))
}

/// Return the canonical directory to put the wasm blobs for providers
pub fn providers_wasm_dir() -> Result<PathBuf, Error> {
    let proj_dirs =
        ProjectDirs::from(QUALIFIER, ORGANIZATION_NAME, APP_NAME).ok_or(Error::ProjDir)?;
    Ok(proj_dirs.data_local_dir().join("providers"))
}
