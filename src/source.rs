//! Loads the SARIF log of an analysis from the hub or from a file.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, bail};

use crate::config::Config;
use crate::hub::Hub;
use crate::sarif::Log;

/// Which analysis to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// The configured analysis: the hub's `analysisId`, else the newest
    /// finished analysis of the hub project, else the SARIF file.
    Configured,
    /// The newest finished analysis of the hub project.
    Latest,
    /// A hub analysis by id.
    Hub(String),
    /// A local SARIF file.
    File(PathBuf),
}

pub struct Loaded {
    pub log: Log,
    /// Identifies the analysis in the memory.
    pub key: String,
    pub name: String,
    pub hub_id: Option<String>,
}

/// Loads the selected analysis. Returns `None` if no source is configured.
pub async fn load(
    config: &Config,
    root: Option<&Path>,
    hub: Option<&Hub>,
    selection: Selection,
) -> anyhow::Result<Option<Loaded>> {
    let selection = match selection {
        Selection::Configured => match (&config.hub, config.sarif_file(root)) {
            (Some(hub), _) if !hub.analysis_id.is_empty() => {
                Selection::Hub(hub.analysis_id.clone())
            }
            (Some(_), _) => Selection::Latest,
            (None, Some(file)) => Selection::File(file),
            (None, None) => return Ok(None),
        },
        selection => selection,
    };

    match selection {
        Selection::File(file) => load_file(&file, root).await.map(Some),
        Selection::Hub(id) => load_hub(hub.context("no hub configured")?, config, &id, None)
            .await
            .map(Some),
        Selection::Latest | Selection::Configured => {
            let hub = hub.context("no hub configured")?;
            let project = config
                .hub
                .as_ref()
                .and_then(|hub| hub.project.clone())
                .context("no hub project configured or detected")?;
            let analyses = hub.analyses(&project).await?;
            let Some(latest) = analyses.into_iter().find(|a| a.is_finished()) else {
                bail!("no finished analysis in hub project {project}");
            };
            load_hub(hub, config, &latest.id.to_string(), Some(latest.name))
                .await
                .map(Some)
        }
    }
}

async fn load_hub(
    hub: &Hub,
    config: &Config,
    id: &str,
    name: Option<String>,
) -> anyhow::Result<Loaded> {
    let log = hub.sarif(id).await?;
    let project = config.hub.as_ref().and_then(|hub| hub.project.as_ref());
    let name = match (name, project) {
        (Some(name), _) => Some(name),
        // The name only makes the analysis easier to recognize.
        (None, Some(project)) => hub.analyses(project).await.ok().and_then(|analyses| {
            analyses
                .into_iter()
                .find(|analysis| analysis.id.to_string() == id)
                .map(|analysis| analysis.name)
        }),
        (None, None) => None,
    };
    let url = hub.config().url.trim_end_matches('/');
    Ok(Loaded {
        log,
        key: format!("hub:{url}#{id}"),
        name: name.unwrap_or_else(|| format!("analysis {id}")),
        hub_id: Some(id.to_string()),
    })
}

async fn load_file(file: &Path, root: Option<&Path>) -> anyhow::Result<Loaded> {
    let file = match root {
        Some(root) if file.is_relative() => root.join(file),
        _ => file.to_path_buf(),
    };
    let text = tokio::fs::read_to_string(&file)
        .await
        .with_context(|| format!("cannot read {}", file.display()))?;
    let log =
        Log::from_json(&text).with_context(|| format!("invalid SARIF in {}", file.display()))?;

    // Without the analysis time, a new export of the same file is told
    // apart by its modification time.
    let version = log
        .runs
        .first()
        .and_then(|run| run.end_time())
        .map(str::to_string)
        .or_else(|| {
            let modified = std::fs::metadata(&file).ok()?.modified().ok()?;
            Some(
                modified
                    .duration_since(UNIX_EPOCH)
                    .ok()?
                    .as_secs()
                    .to_string(),
            )
        })
        .unwrap_or_default();
    let name = root
        .and_then(|root| file.strip_prefix(root).ok())
        .unwrap_or(&file)
        .display()
        .to_string();

    Ok(Loaded {
        log,
        key: format!("file:{}@{version}", file.display()),
        name,
        hub_id: None,
    })
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::HubConfig;

    const FIXTURE: &str = include_str!("../tests/fixtures/codesonar.sarif");

    async fn configured(config: &Config, root: Option<&Path>) -> anyhow::Result<Option<Loaded>> {
        let hub = config.hub.as_ref().map(|hub| Hub::new(hub).unwrap());
        load(config, root, hub.as_ref(), Selection::Configured).await
    }

    #[tokio::test]
    async fn loads_relative_sarif_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("warnings.sarif"), FIXTURE).unwrap();

        let config = Config {
            sarif_file: Some("warnings.sarif".into()),
            ..Config::default()
        };

        let loaded = configured(&config, Some(dir.path()))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded.log.runs[0].results.len(), 4);
        assert_eq!(loaded.name, "warnings.sarif");
        assert_eq!(loaded.hub_id, None);
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

        let loaded = configured(&config, None).await.unwrap().unwrap();

        assert_eq!(loaded.key, format!("hub:{}#1", server.uri()));
        assert_eq!(loaded.hub_id.as_deref(), Some("1"));
    }

    #[tokio::test]
    async fn reports_missing_and_invalid_files() {
        let dir = tempfile::tempdir().unwrap();

        let config = Config {
            sarif_file: Some(dir.path().join("missing.sarif")),
            ..Config::default()
        };

        let error = configured(&config, None).await.err().unwrap();

        assert!(error.to_string().contains("cannot read"), "{error}");

        let file = dir.path().join("broken.sarif");
        std::fs::write(&file, "{").unwrap();

        let config = Config {
            sarif_file: Some(file),
            ..Config::default()
        };

        let error = configured(&config, None).await.err().unwrap();

        assert!(error.to_string().contains("invalid SARIF"), "{error}");
    }

    #[tokio::test]
    async fn returns_none_without_source() {
        assert!(
            configured(&Config::default(), None)
                .await
                .unwrap()
                .is_none()
        );
    }
}
