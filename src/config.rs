//! Server settings, sent by the client as `initializationOptions`
//! or through `workspace/didChangeConfiguration`.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

/// Environment variable used when no hub password is configured.
pub const PASSWORD_ENV: &str = "CODESONAR_PASSWORD";

const DEFAULT_SARIF_PATH: &str = "/analysis/{analysisId}-allwarnings.sarif";

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// Exported SARIF file, absolute or relative to the workspace root.
    pub sarif_file: Option<PathBuf>,
    /// CodeSonar hub to download the warnings from.
    pub hub: Option<HubConfig>,
    /// Rewrites paths from the analysis machine to local paths.
    pub path_mappings: Vec<PathMapping>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HubConfig {
    pub url: String,
    pub analysis_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Hub path of the SARIF export; `{analysisId}` is replaced.
    pub sarif_path: Option<String>,
    /// PEM file with an additional trusted CA certificate.
    pub ca_certificate: Option<PathBuf>,
    pub accept_invalid_certs: bool,
}

impl HubConfig {
    pub fn sarif_path(&self) -> String {
        self.sarif_path
            .as_deref()
            .unwrap_or(DEFAULT_SARIF_PATH)
            .replace("{analysisId}", &self.analysis_id)
    }

    /// The configured password, falling back to `CODESONAR_PASSWORD`.
    pub fn password(&self) -> Option<String> {
        self.password
            .clone()
            .or_else(|| std::env::var(PASSWORD_ENV).ok())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PathMapping {
    pub from: String,
    pub to: String,
}

impl PathMapping {
    /// Replaces the `from` prefix of `path` with `to`.
    pub fn apply(&self, path: &str) -> Option<String> {
        let from = normalize(&self.from);
        let from = from.trim_end_matches('/');
        let rest = normalize(path).strip_prefix(from)?.to_string();
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        Some(format!("{}{}", self.to.trim_end_matches(['/', '\\']), rest))
    }
}

/// Uses forward slashes and drops the leading slash of `/C:/...` paths.
pub fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path[1..].to_string()
    } else {
        path
    }
}

impl Config {
    /// Parses settings, which may be nested under a `codesonar` key.
    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let value = value.get("codesonar").unwrap_or(value);
        if value.is_null() {
            return Ok(Self::default());
        }
        Ok(Self::deserialize(value)?)
    }

    pub fn sarif_file(&self, root: Option<&Path>) -> Option<PathBuf> {
        let file = self.sarif_file.as_ref()?;
        match root {
            Some(root) if file.is_relative() => Some(root.join(file)),
            _ => Some(file.clone()),
        }
    }

    pub fn map_path(&self, path: &str) -> String {
        self.path_mappings
            .iter()
            .find_map(|mapping| mapping.apply(path))
            .unwrap_or_else(|| path.to_string())
    }
}
