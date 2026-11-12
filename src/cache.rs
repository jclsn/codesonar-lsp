//! Downloaded SARIF exports of finished hub analyses. The hub creates them
//! while sending them, which takes a minute for large analyses, and they
//! do not change once the analysis is finished.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

use crate::config::Config;

/// How many analyses of a project are kept by default.
pub const DEFAULT_SIZE: usize = 10;

/// The cached analyses of a hub project, the most recently used ones.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
    size: usize,
}

impl Cache {
    pub fn new(dir: PathBuf, size: usize) -> Self {
        Self { dir, size }
    }

    /// The cache of the hub project of the settings, if there is one.
    pub fn of(config: &Config) -> Option<Self> {
        let hub = config.hub.as_ref()?;
        let project = hub.project.as_deref()?;
        let root = config.cache_dir.clone().or_else(default_dir)?;
        let size = config.cached_analyses.unwrap_or(DEFAULT_SIZE);
        Some(Self::new(
            root.join(name(&hub.url)).join(name(project)),
            size,
        ))
    }

    /// The SARIF export of an analysis, if it is cached.
    pub fn get(&self, analysis_id: &str) -> Option<String> {
        let file = self.file(analysis_id)?;
        let text = std::fs::read_to_string(&file).ok()?;
        // The least recently used analyses are dropped first.
        let _ = std::fs::File::options()
            .append(true)
            .open(&file)
            .and_then(|file| file.set_modified(SystemTime::now()));
        Some(text)
    }

    pub fn contains(&self, analysis_id: &str) -> bool {
        self.file(analysis_id).is_some_and(|file| file.is_file())
    }

    /// Keeps the SARIF export of an analysis and drops the least recently
    /// used ones beyond the size of the cache.
    pub fn put(&self, analysis_id: &str, text: &str) -> anyhow::Result<()> {
        let Some(file) = self.file(analysis_id) else {
            return Ok(());
        };

        if self.size == 0 {
            return Ok(());
        }

        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        // Written completely or not at all, in case the server stops.
        let partial = file.with_extension("sarif.partial");
        std::fs::write(&partial, text)
            .and_then(|()| std::fs::rename(&partial, &file))
            .with_context(|| format!("cannot write {}", file.display()))?;
        self.evict();
        Ok(())
    }

    /// Drops an analysis. Returns whether it was cached.
    pub fn remove(&self, analysis_id: &str) -> anyhow::Result<bool> {
        let Some(file) = self.file(analysis_id).filter(|file| file.is_file()) else {
            return Ok(false);
        };

        std::fs::remove_file(&file).with_context(|| format!("cannot delete {}", file.display()))?;
        Ok(true)
    }

    /// The file of an analysis. Hub ids are numbers, so other ids cannot
    /// lead out of the directory.
    fn file(&self, analysis_id: &str) -> Option<PathBuf> {
        (!analysis_id.is_empty() && analysis_id.bytes().all(|b| b.is_ascii_digit()))
            .then(|| self.dir.join(format!("{analysis_id}.sarif")))
    }

    fn evict(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(SystemTime, PathBuf)> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                let used = path.metadata().ok()?.modified().ok()?;
                (path.extension()? == "sarif").then_some((used, path))
            })
            .collect();
        files.sort_by_key(|(used, _)| std::cmp::Reverse(*used));

        for (_, file) in files.into_iter().skip(self.size) {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// A directory name for a hub URL or project name.
fn name(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '.' => c,
            _ => '_',
        })
        .collect()
}

/// The default directory of the cache: `$XDG_CACHE_HOME/codesonar-lsp`,
/// else `~/.cache/codesonar-lsp`.
pub fn default_dir() -> Option<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))?;
    Some(cache.join("codesonar-lsp"))
}
