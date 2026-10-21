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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_nested_settings() {
        let value = json!({"codesonar": {
            "sarifFile": "out/warnings.sarif",
            "hub": {"url": "https://hub:7340", "analysisId": "42"},
            "pathMappings": [{"from": "/build", "to": "/home/me/src"}]
        }});
        let config = Config::from_value(&value).unwrap();
        assert_eq!(config.sarif_file, Some(PathBuf::from("out/warnings.sarif")));
        let hub = config.hub.unwrap();
        assert_eq!(hub.url, "https://hub:7340");
        assert_eq!(hub.sarif_path(), "/analysis/42-allwarnings.sarif");
        assert_eq!(config.path_mappings.len(), 1);
    }

    #[test]
    fn parses_flat_and_empty_settings() {
        let config = Config::from_value(&json!({"sarifFile": "a.sarif"})).unwrap();
        assert_eq!(config.sarif_file, Some(PathBuf::from("a.sarif")));
        assert_eq!(Config::from_value(&Value::Null).unwrap(), Config::default());
        assert!(Config::from_value(&json!({"sarifFile": 3})).is_err());
    }

    #[test]
    fn uses_custom_sarif_path() {
        let hub = HubConfig {
            analysis_id: "7".into(),
            sarif_path: Some("/analysis/{analysisId}-allwarnings.sarif?filter=2".into()),
            ..HubConfig::default()
        };
        assert_eq!(hub.sarif_path(), "/analysis/7-allwarnings.sarif?filter=2");
    }

    #[test]
    fn resolves_relative_sarif_file() {
        let config = Config {
            sarif_file: Some("w.sarif".into()),
            ..Config::default()
        };
        let root = Path::new("/ws");
        assert_eq!(config.sarif_file(Some(root)), Some("/ws/w.sarif".into()));
        assert_eq!(config.sarif_file(None), Some("w.sarif".into()));
    }

    #[test]
    fn maps_paths() {
        let config = Config {
            path_mappings: vec![
                PathMapping {
                    from: "C:\\build\\".into(),
                    to: "/home/me/src".into(),
                },
                PathMapping {
                    from: "/build".into(),
                    to: "/src/".into(),
                },
            ],
            ..Config::default()
        };
        assert_eq!(config.map_path("/C:/build/a/b.c"), "/home/me/src/a/b.c");
        assert_eq!(config.map_path("C:\\build\\b.c"), "/home/me/src/b.c");
        assert_eq!(config.map_path("/build/x.c"), "/src/x.c");
        assert_eq!(config.map_path("/buildx/x.c"), "/buildx/x.c");
        assert_eq!(config.map_path("/other/x.c"), "/other/x.c");
    }

    #[test]
    fn normalizes_windows_paths() {
        assert_eq!(normalize("/C:/a"), "C:/a");
        assert_eq!(normalize("D:\\a\\b"), "D:/a/b");
        assert_eq!(normalize("/usr/a"), "/usr/a");
    }
}
