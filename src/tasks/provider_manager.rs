//! Tasks to handle provider management

use crate::runtime::providers_wasm_dir;
use clap::Parser;
use duct::cmd;
use fiberplane::provider_bindings::Timestamp;
use flate2::read::GzDecoder;
use http_body_util::BodyExt;
use octocrab::models::{ArtifactId, JobId, RepositoryId};
use octocrab::{Octocrab, Page};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use tar::Archive;
use thiserror::Error;

const GITHUB_BASE_URI: &str = "https://api.github.com";

#[derive(Debug, Error)]
pub enum Error {
    #[error("Runtime error: {0}")]
    Runtime(#[from] crate::runtime::Error),
    #[error("GitHub error: {0}")]
    GitHub(#[from] octocrab::Error),
    #[error("HTTP error: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("Zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("Artifacts expired for branch {branch_name}")]
    ArtifactsExpired { branch_name: String },
    #[error("Specified release or branch could not be found or has no assets")]
    ProvidersNotFound,
    #[error("A GitHub token is required for pulling providers from a branch")]
    TokenRequired,
    #[error("{message}")]
    Other { message: String },
}

#[derive(Parser)]
pub struct PullProvidersArgs {
    /// The branch for which to fetch the provider artifacts.
    ///
    /// Fetching from a branch requires the GitHub token to be set.
    #[clap(short, long)]
    branch: Option<String>,

    /// Token to use when fetching artifacts from a branch instead of a release.
    #[clap(long, env)]
    github_token: Option<secrecy::SecretString>,

    /// The `providers` release to fetch.
    ///
    /// If the value "latest" is given (the default), it fetches the latest
    /// release instead of looking for a specific tag.
    ///
    /// This argument is ignored if a branch is specified.
    #[clap(short, long, default_value = "latest")]
    release: String,

    /// Path to directory to store the WASM files.
    #[clap(long, env)]
    wasm_dir: Option<PathBuf>,
}

pub async fn pull(args: PullProvidersArgs) -> Result<(), Error> {
    let wasm_dir = &args.wasm_dir;
    ensure_wasm_dir(wasm_dir)?;

    if let Some(branch) = args.branch.as_ref() {
        let github_token = args.github_token.ok_or(Error::TokenRequired)?;
        let octocrab = Octocrab::builder()
            .personal_token(github_token.expose_secret().clone())
            .build()
            .map_err(|err| Error::Other {
                message: format!("Could not create GitHub client: {err}"),
            })?;
        download_provider_artifacts_from_branch(&octocrab, branch, wasm_dir).await
    } else {
        download_providers_release(&Octocrab::default(), &args.release, wasm_dir).await
    }
}

fn ensure_wasm_dir(wasm_dir: &Option<PathBuf>) -> Result<(), Error> {
    let wasm_dir = match wasm_dir {
        Some(wasm_dir) => wasm_dir.clone(),
        None => providers_wasm_dir()?,
    };

    if wasm_dir.exists() && !wasm_dir.is_dir() {
        return Err(Error::Runtime(
            crate::runtime::Error::ProvidersDirUnavailable(wasm_dir),
        ));
    }

    if !wasm_dir.exists() {
        std::fs::create_dir_all(wasm_dir)?;
    }

    Ok(())
}

async fn download_provider_artifacts_from_branch(
    octocrab: &Octocrab,
    branch: &str,
    wasm_dir: &Option<PathBuf>,
) -> Result<(), Error> {
    eprintln!("Finding latest provider artifacts...");

    let latest_artifact = fetch_latest_artifact(octocrab, branch).await?;
    if latest_artifact.expired {
        return Err(Error::ArtifactsExpired {
            branch_name: branch.to_owned(),
        });
    }

    // latest_artifact.
    let download_url = &latest_artifact.archive_download_url;
    download_providers_archive(octocrab, download_url, wasm_dir).await
}

async fn download_providers_release(
    octocrab: &Octocrab,
    release: &str,
    wasm_dir: &Option<PathBuf>,
) -> Result<(), Error> {
    let release = if release == "latest" {
        eprintln!("Fetching latest providers release...");

        octocrab
            .repos("fiberplane", "providers")
            .releases()
            .get_latest()
            .await?
    } else {
        eprintln!("Fetching providers release {release}...",);

        octocrab
            .repos("fiberplane", "providers")
            .releases()
            .get_by_tag(release)
            .await?
    };

    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == "providers.tgz")
        .ok_or(Error::ProvidersNotFound)?;

    download_providers_archive(octocrab, asset.browser_download_url.as_str(), wasm_dir).await
}

async fn download_providers_archive(
    octocrab: &Octocrab,
    download_url: &str,
    wasm_dir: &Option<PathBuf>,
) -> Result<(), Error> {
    if download_url.ends_with("zip") {
        download_providers_zip(octocrab, download_url, wasm_dir).await?
    } else {
        download_providers_tarball(octocrab, download_url, wasm_dir).await?
    }

    eprintln!("Providers updated.");

    Ok(())
}

async fn download_providers_tarball(
    octocrab: &Octocrab,
    download_url: &str,
    wasm_dir: &Option<PathBuf>,
) -> Result<(), Error> {
    eprintln!("Downloading providers tarball from: {download_url}");

    // We need to get rid of the base URL, to ensure that octocrab will add
    // authentication headers again.
    let download_url = download_url.trim_start_matches(GITHUB_BASE_URI);

    let response = octocrab._get(download_url).await?;
    let tarball_bytes = if response.status().is_redirection() {
        let header_value = response.headers().get("location").unwrap();
        let location = header_value.to_str().unwrap().to_string();

        if location.starts_with(GITHUB_BASE_URI) {
            let location = location.trim_start_matches(GITHUB_BASE_URI);
            let response = octocrab._get(location).await?;
            response.into_body().collect().await?.to_bytes()
        } else {
            reqwest::get(location).await?.bytes().await?
        }
    } else {
        response.into_body().collect().await?.to_bytes()
    };

    let mut archive = Archive::new(GzDecoder::new(Cursor::new(tarball_bytes)));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if match path.extension() {
            Some(ext) => ext != "wasm",
            None => true,
        } {
            continue;
        }

        let provider = path
            .file_stem()
            .ok_or_else(|| Error::Other {
                message: "Cannot determine provider name from archived artifact".to_owned(),
            })?
            .to_owned();

        entry.unpack(&get_provider_destination(wasm_dir, &provider))?;
    }

    Ok(())
}

async fn download_providers_zip(
    octocrab: &Octocrab,
    download_url: &str,
    wasm_dir: &Option<PathBuf>,
) -> Result<(), Error> {
    eprintln!("Downloading providers zip from: {download_url}");

    // We need to get rid of the base URL, to ensure that octocrab will add
    // authentication headers again.
    let download_url = download_url.trim_start_matches(GITHUB_BASE_URI);

    let response = octocrab._get(download_url).await?;
    let zip_bytes = if response.status().is_redirection() {
        let header_value = response.headers().get("location").unwrap();
        let location = header_value.to_str().unwrap().to_string();

        if location.starts_with(GITHUB_BASE_URI) {
            let location = location.trim_start_matches(GITHUB_BASE_URI);
            let response = octocrab._get(location).await?;
            response.into_body().collect().await?.to_bytes()
        } else {
            reqwest::get(location).await?.bytes().await?
        }
    } else {
        response.into_body().collect().await?.to_bytes()
    };

    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let provider = Path::new(file.name())
            .file_stem()
            .ok_or_else(|| Error::Other {
                message: "Cannot determine provider name from zipped artifact".to_owned(),
            })?
            .to_owned();
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;
        fs::write(get_provider_destination(wasm_dir, &provider), buffer)?;
    }

    Ok(())
}

async fn fetch_latest_artifact(octocrab: &Octocrab, branch: &str) -> Result<Artifact, Error> {
    let artifacts = get_artifacts(octocrab, "fiberplane-providers").await?;
    artifacts
        .items
        .iter()
        .find(|artifact| match &artifact.workflow_run {
            Some(run) => run.head_branch == branch,
            None => false,
        })
        .ok_or(Error::ProvidersNotFound)
        .cloned()
}

async fn get_artifacts(octocrab: &Octocrab, artifact_name: &str) -> Result<Page<Artifact>, Error> {
    octocrab
        .get(
            "/repos/fiberplane/providers/actions/artifacts",
            Some(&ArtifactsParams {
                name: artifact_name.to_owned(),
            }),
        )
        .await
        .map_err(Error::from)
}

#[derive(Parser)]
pub struct BuildProvidersArgs {
    /// Keep debugging information in the built provider(s).
    #[clap(short, long)]
    debug: bool,

    /// A specific provider to build. If omitted, all providers are built.
    #[clap(long, default_value = "all")]
    provider: String,

    /// Path where the checkout of the `providers` repository can be found.
    #[clap(long, default_value = "../providers")]
    providers_dir: PathBuf,

    /// Path to directory to store the WASM files.
    #[clap(long, env)]
    wasm_dir: Option<PathBuf>,
}

pub fn build_providers(args: BuildProvidersArgs) -> Result<(), Error> {
    let wasm_dir = &args.wasm_dir;
    ensure_wasm_dir(wasm_dir)?;

    let providers_dir = args.providers_dir;
    if !providers_dir.exists() {
        return Err(Error::Other {
            message: format!(
                "Providers directory `{}` doesn't exist",
                providers_dir.display()
            ),
        });
    }

    let provider = &args.provider;
    let mut cargo_args = vec!["xtask", "build", provider];
    if args.debug {
        cargo_args.push("--debug");
    }
    cmd("cargo", cargo_args).dir(&providers_dir).run()?;

    if provider == "all" {
        copy_all_artifacts(providers_dir.join("artifacts"), wasm_dir)
    } else {
        let artifact = providers_dir.join(format!("artifacts/{provider}.wasm"));
        fs::copy(
            artifact,
            get_provider_destination(wasm_dir, OsStr::new(provider)),
        )?;
        Ok(())
    }
}

fn copy_all_artifacts(
    artifacts_path: impl AsRef<Path>,
    wasm_dir: &Option<PathBuf>,
) -> Result<(), Error> {
    for entry in fs::read_dir(artifacts_path)? {
        let path = entry?.path();
        if path.extension().unwrap_or_default() != "wasm" {
            continue;
        }

        fs::copy(
            &path,
            get_provider_destination(
                wasm_dir,
                path.file_stem().ok_or_else(|| Error::Other {
                    message: "Artifact had no filename".to_owned(),
                })?,
            ),
        )?;
    }

    Ok(())
}

fn get_provider_destination(wasm_dir: &Option<PathBuf>, provider_name: &OsStr) -> PathBuf {
    let mut provider_filename = OsString::from(provider_name);
    provider_filename.push(".wasm");

    let mut destination_path = match wasm_dir {
        Some(wasm_dir) => wasm_dir.clone(),
        None => providers_wasm_dir().unwrap(),
    };
    destination_path.push(provider_filename);
    destination_path
}

/// (Incomplete) representation of a Github artifact.
#[derive(Clone, Debug, Deserialize)]
pub struct Artifact {
    /// The ID of the artifact.
    pub id: ArtifactId,

    pub expired: bool,

    /// The name of the artifact.
    pub name: Option<String>,

    pub node_id: Option<String>,

    pub size_in_bytes: u64,

    pub url: String,

    pub archive_download_url: String,

    pub created_at: Timestamp,

    pub updated_at: Timestamp,

    pub expires_at: Option<Timestamp>,

    pub workflow_run: Option<WorkflowRunSummary>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct WorkflowRunSummary {
    pub id: JobId,

    pub repository_id: RepositoryId,

    pub head_branch: String,

    /// The SHA of the head commit that points to the version of the workflow being run.
    pub head_sha: String,
}

#[derive(Clone, Serialize)]
struct ArtifactsParams {
    name: String,
}
