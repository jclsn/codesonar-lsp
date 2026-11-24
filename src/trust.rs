//! The hubs that may get the user's credentials.
//!
//! A repository's `codesonar.conf` can name any hub in `HUB_ADDRESS`. So
//! that opening a repository does not send the credentials to a hub the
//! user does not know, the server asks before signing in to a hub that the
//! settings do not name, and remembers the hubs the user trusts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::private;

#[derive(Debug, Default)]
pub struct Trust {
    file: Option<PathBuf>,
    hubs: BTreeSet<String>,
}

impl Trust {
    /// Opens the trusted hubs in `dir`. A missing or damaged file trusts
    /// none.
    pub fn open(dir: &Path) -> Self {
        let file = dir.join("trusted_hubs.json");
        let hubs = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            file: Some(file),
            hubs,
        }
    }

    /// Whether the user trusts the hub at `address`, like `host:port`.
    pub fn contains(&self, address: &str) -> bool {
        self.hubs.contains(&normalize(address))
    }

    /// Trusts the hub at `address` from now on.
    pub fn add(&mut self, address: &str) -> anyhow::Result<()> {
        self.hubs.insert(normalize(address));

        let Some(file) = &self.file else {
            return Ok(());
        };

        if let Some(dir) = file.parent() {
            private::create_dir(dir)?;
        }

        std::fs::write(file, serde_json::to_string(&self.hubs)?)
            .with_context(|| format!("cannot write {}", file.display()))
    }
}

/// Host names are case-insensitive.
fn normalize(address: &str) -> String {
    address.trim().trim_end_matches('/').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_trusted_hubs() {
        let dir = tempfile::tempdir().unwrap();
        let mut trust = Trust::open(dir.path());

        assert!(!trust.contains("hub:7340"));

        trust.add("Hub:7340").unwrap();

        let trust = Trust::open(dir.path());
        assert!(trust.contains("hub:7340"), "host names ignore case");
        assert!(!trust.contains("hub:7341"));
    }

    #[test]
    fn trusts_none_with_a_damaged_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("trusted_hubs.json"), "{").unwrap();

        assert!(!Trust::open(dir.path()).contains("hub:7340"));
    }
}
