//! Remembers the analyzed code of each warning between sessions.
//!
//! Once a file is edited, the analyzed code cannot be read from disk any
//! more. The memory keeps it, keyed by analysis and warning fingerprint,
//! so warnings can still be matched to the current code later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::tracking::Anchor;

/// Analyses beyond this number are forgotten, least recently used first.
const LIMIT: usize = 50;
const VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    version: u32,
    analyses: HashMap<String, Entry>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Entry {
    used: u64,
    anchors: HashMap<String, Anchor>,
}

#[derive(Debug)]
pub struct Memory {
    file: PathBuf,
    data: Data,
}

impl Memory {
    /// Opens the memory in `dir`. A missing or damaged memory is only a
    /// cache, so it starts out empty then.
    pub fn open(dir: &Path) -> Self {
        let file = dir.join("memory.json");
        let data = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str::<Data>(&text).ok())
            .filter(|data| data.version == VERSION)
            .unwrap_or(Data {
                version: VERSION,
                analyses: HashMap::new(),
            });
        Self { file, data }
    }

    /// The remembered code per fingerprint for an analysis.
    pub fn anchors(&self, key: &str) -> HashMap<String, Anchor> {
        self.data
            .analyses
            .get(key)
            .map(|entry| entry.anchors.clone())
            .unwrap_or_default()
    }

    /// Adds anchors for an analysis and writes the memory.
    pub fn remember(&mut self, key: &str, anchors: HashMap<String, Anchor>) -> anyhow::Result<()> {
        let entry = self.data.analyses.entry(key.to_string()).or_default();
        entry.anchors.extend(anchors);
        entry.used = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_millis() as u64);

        if self.data.analyses.len() > LIMIT {
            let mut keys: Vec<(u64, String)> = self
                .data
                .analyses
                .iter()
                .map(|(key, entry)| (entry.used, key.clone()))
                .collect();
            keys.sort();

            for (_, key) in keys.iter().take(keys.len() - LIMIT) {
                self.data.analyses.remove(key);
            }
        }

        self.save()
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }

        std::fs::write(&self.file, serde_json::to_string(&self.data)?)
            .with_context(|| format!("cannot write {}", self.file.display()))
    }
}

/// The default directory of the memory: `$XDG_STATE_HOME/codesonar-lsp`,
/// else `~/.local/state/codesonar-lsp`.
pub fn default_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(state.join("codesonar-lsp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(line: &str) -> Anchor {
        Anchor {
            line: line.into(),
            before: String::new(),
            after: String::new(),
        }
    }

    #[test]
    fn keeps_anchors_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = Memory::open(dir.path());

        assert!(memory.anchors("a").is_empty());

        memory
            .remember("a", [("fp1".into(), anchor("x"))].into())
            .unwrap();
        memory
            .remember("a", [("fp2".into(), anchor("y"))].into())
            .unwrap();

        let memory = Memory::open(dir.path());
        let anchors = memory.anchors("a");

        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors["fp1"], anchor("x"));
        assert!(memory.anchors("b").is_empty());
    }

    #[test]
    fn forgets_least_recently_used_analyses() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = Memory::open(dir.path());

        for i in 0..=LIMIT {
            memory
                .remember(
                    &format!("analysis {i}"),
                    [("fp".into(), anchor("x"))].into(),
                )
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let memory = Memory::open(dir.path());
        assert!(memory.anchors("analysis 0").is_empty());
        assert_eq!(memory.anchors("analysis 1").len(), 1);
        assert_eq!(memory.anchors(&format!("analysis {LIMIT}")).len(), 1);
    }

    #[test]
    fn starts_over_with_damaged_memory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("memory.json"), "{").unwrap();

        assert!(Memory::open(dir.path()).anchors("a").is_empty());

        std::fs::write(
            dir.path().join("memory.json"),
            r#"{"version":2,"analyses":{}}"#,
        )
        .unwrap();

        let mut memory = Memory::open(dir.path());
        memory.remember("a", HashMap::new()).unwrap();

        assert!(Memory::open(dir.path()).data.analyses.contains_key("a"));
    }
}
