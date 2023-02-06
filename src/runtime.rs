//! Utility functions that allow runtime configuration behaviour

use anyhow::anyhow;
use directories::ProjectDirs;
use std::path::PathBuf;

pub const QUALIFIER: &str = "dev";
pub const ORGANIZATION_NAME: &str = "fiberplane";
pub const APP_NAME: &str = "fpd";

/// Return the canonical path to put the sources configuration file
pub fn data_sources_path() -> Result<PathBuf, anyhow::Error> {
    let proj_dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION_NAME, APP_NAME)
        .ok_or_else(|| anyhow!("No project directory is found on this OS."))?;
    Ok(proj_dirs.config_dir().join("data_sources.yaml"))
}

/// Return the canonical directory to put the wasm blobs for providers
pub fn providers_wasm_dir() -> Result<PathBuf, anyhow::Error> {
    let proj_dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION_NAME, APP_NAME)
        .ok_or_else(|| anyhow!("No project directory is found on this OS."))?;
    Ok(proj_dirs.data_local_dir().join("providers"))
}
