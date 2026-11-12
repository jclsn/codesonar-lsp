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
    /// How warnings in code changed since the analysis are shown.
    pub changed: Changed,
    /// Directory of the memory of analyzed code.
    pub state_dir: Option<PathBuf>,
    /// How to run an analysis.
    pub analyze: AnalyzeConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AnalyzeConfig {
    /// The CodeSonar command.
    pub command: String,
    /// Project prefix, relative to the workspace root. Detected from the
    /// project files of the workspace if not set.
    pub project_file: Option<PathBuf>,
    /// Build command, as a list or a string split at whitespace.
    #[serde(deserialize_with = "words")]
    pub build_command: Vec<String>,
    /// Configuration file with the settings of the workspace, relative to
    /// its root. Passed with `-conf-file` if it exists.
    pub conf_file: PathBuf,
    /// Hub project path. Defaults to `/` and the hub project.
    pub project: Option<String>,
    /// CodeSonar presets of analyses, e.g. `misrac2025`.
    pub presets: Vec<String>,
    /// Further arguments of `codesonar analyze`.
    pub args: Vec<String>,
}

impl Default for AnalyzeConfig {
    fn default() -> Self {
        Self {
            command: "codesonar".into(),
            project_file: None,
            build_command: vec!["make".into(), "-B".into()],
            conf_file: "codesonar.conf".into(),
            project: None,
            presets: Vec::new(),
            args: Vec::new(),
        }
    }
}

fn words<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Words {
        Line(String),
        List(Vec<String>),
    }

    Ok(match Words::deserialize(deserializer)? {
        Words::Line(line) => line.split_whitespace().map(str::to_string).collect(),
        Words::List(list) => list,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Changed {
    /// Leave them out: they are usually fixed.
    #[default]
    Hide,
    /// Show them as hints.
    Mark,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HubConfig {
    pub url: String,
    /// Analysis to show. Without one, the newest finished analysis of
    /// `project` is shown.
    pub analysis_id: String,
    /// Hub project, by name or id.
    pub project: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// File whose first line is the password.
    pub password_file: Option<PathBuf>,
    /// Hub path of the SARIF export; `{analysisId}` is replaced.
    pub sarif_path: Option<String>,
    /// PEM file with an additional trusted CA certificate.
    pub ca_certificate: Option<PathBuf>,
    pub accept_invalid_certs: bool,
}

impl HubConfig {
    pub fn sarif_path(&self, analysis_id: &str) -> String {
        self.sarif_path
            .as_deref()
            .unwrap_or(DEFAULT_SARIF_PATH)
            .replace("{analysisId}", analysis_id)
    }

    /// The configured password, else the first line of the password file,
    /// else `CODESONAR_PASSWORD`.
    pub fn password(&self) -> Option<String> {
        if let Some(password) = &self.password {
            return Some(password.clone());
        }

        if let Some(file) = &self.password_file {
            let text = std::fs::read_to_string(expand_home(file)).ok()?;
            return Some(text.lines().next().unwrap_or_default().to_string());
        }

        std::env::var(PASSWORD_ENV).ok()
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

/// Replaces a leading `~` with the home directory.
pub fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
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
        assert_eq!(
            hub.sarif_path(&hub.analysis_id),
            "/analysis/42-allwarnings.sarif"
        );
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
        assert_eq!(
            hub.sarif_path(&hub.analysis_id),
            "/analysis/7-allwarnings.sarif?filter=2"
        );
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
    fn reads_password_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("password");
        std::fs::write(&file, "secret\nignored\n").unwrap();

        let hub = HubConfig {
            password_file: Some(file.clone()),
            ..HubConfig::default()
        };
        assert_eq!(hub.password().as_deref(), Some("secret"));

        let hub = HubConfig {
            password: Some("direct".into()),
            ..hub
        };
        assert_eq!(hub.password().as_deref(), Some("direct"));

        let hub = HubConfig {
            password_file: Some(dir.path().join("missing")),
            ..HubConfig::default()
        };
        assert_eq!(hub.password(), None);
    }

    #[test]
    fn expands_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(expand_home(Path::new("~/a")), Path::new(&home).join("a"));
        assert_eq!(expand_home(Path::new("/a/~")), Path::new("/a/~"));
    }

    #[test]
    fn parses_analyze_settings() {
        let config = Config::from_value(&json!({"changed": "mark"})).unwrap();
        assert_eq!(config.changed, Changed::Mark);
        assert_eq!(config.analyze, AnalyzeConfig::default());

        let value = json!({"analyze": {"buildCommand": ["cmake", "--build", "out dir"]}});
        let config = Config::from_value(&value).unwrap();

        assert_eq!(
            config.analyze.build_command,
            ["cmake", "--build", "out dir"]
        );
        assert_eq!(config.analyze.command, "codesonar", "defaults kept");
        assert_eq!(config.analyze.conf_file, Path::new("codesonar.conf"));
        assert!(Config::from_value(&json!({"changed": "sometimes"})).is_err());
    }

    #[test]
    fn normalizes_windows_paths() {
        assert_eq!(normalize("/C:/a"), "C:/a");
        assert_eq!(normalize("D:\\a\\b"), "D:/a/b");
        assert_eq!(normalize("/usr/a"), "/usr/a");
    }
}
