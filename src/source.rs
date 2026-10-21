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
