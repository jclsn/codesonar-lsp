//! Loads warnings from the configured source.

use std::path::Path;

use anyhow::Context;

use crate::config::Config;
use crate::hub::Hub;
use crate::sarif::Log;
use crate::warning::{self, Warning};

/// Loads the warnings from the hub if configured, otherwise from the SARIF file.
///
/// Returns `None` if no source is configured.
pub async fn load(config: &Config, root: Option<&Path>) -> anyhow::Result<Option<Vec<Warning>>> {
    let log = if let Some(hub) = &config.hub {
        Hub::new(hub)?.fetch_sarif().await?
    } else if let Some(file) = config.sarif_file(root) {
        let text = tokio::fs::read_to_string(&file)
            .await
            .with_context(|| format!("cannot read {}", file.display()))?;
        Log::from_json(&text).with_context(|| format!("invalid SARIF in {}", file.display()))?
    } else {
        return Ok(None);
    };
    Ok(Some(warning::from_log(&log, config, root)))
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::HubConfig;

    const FIXTURE: &str = include_str!("../tests/fixtures/codesonar.sarif");

    #[tokio::test]
    async fn loads_relative_sarif_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("warnings.sarif"), FIXTURE).unwrap();
        let config = Config {
            sarif_file: Some("warnings.sarif".into()),
            ..Config::default()
        };
        let warnings = load(&config, Some(dir.path())).await.unwrap().unwrap();
        assert_eq!(warnings.len(), 3);
    }

    #[tokio::test]
    async fn prefers_hub_over_file() {
        let server = MockServer::start().await;
        Mock::given(path("/analysis/1-allwarnings.sarif"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
            .expect(1)
            .mount(&server)
            .await;
        let config = Config {
            sarif_file: Some("/does/not/exist.sarif".into()),
            hub: Some(HubConfig {
                url: server.uri(),
                analysis_id: "1".into(),
                ..HubConfig::default()
            }),
            ..Config::default()
        };
        let warnings = load(&config, None).await.unwrap().unwrap();
        assert_eq!(warnings.len(), 3);
    }

    #[tokio::test]
    async fn reports_missing_and_invalid_files() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            sarif_file: Some(dir.path().join("missing.sarif")),
            ..Config::default()
        };
        let error = load(&config, None).await.unwrap_err();
        assert!(error.to_string().contains("cannot read"), "{error}");

        let file = dir.path().join("broken.sarif");
        std::fs::write(&file, "{").unwrap();
        let config = Config {
            sarif_file: Some(file),
            ..Config::default()
        };
        let error = load(&config, None).await.unwrap_err();
        assert!(error.to_string().contains("invalid SARIF"), "{error}");
    }

    #[tokio::test]
    async fn returns_none_without_source() {
        assert!(load(&Config::default(), None).await.unwrap().is_none());
    }
}
