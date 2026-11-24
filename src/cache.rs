//! Downloaded SARIF exports of finished hub analyses. The hub creates them
//! while sending them, which takes a minute for large analyses, and they
//! do not change once the analysis is finished.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

use crate::config::Config;
use crate::private;

/// How many analyses of a project are kept by default.
pub const DEFAULT_SIZE: usize = 10;

/// The cached analyses of a hub project, the most recently used ones.
#[derive(Debug, Clone)]
pub struct Cache {
    /// The directory of all cached analyses, which only the user may read.
    root: PathBuf,
    dir: PathBuf,
    size: usize,
}

impl Cache {
    pub fn new(dir: PathBuf, size: usize) -> Self {
        Self {
            root: dir.clone(),
            dir,
            size,
        }
    }

    /// The cache of the hub project of the settings, if there is one.
    pub fn of(config: &Config) -> Option<Self> {
        let hub = config.hub.as_ref()?;
        let project = hub.project.as_deref()?;
        let root = config.cache_dir.clone().or_else(default_dir)?;
        let size = config.cached_analyses.unwrap_or(DEFAULT_SIZE);
        Some(Self {
            dir: root.join(name(&hub.url)).join(name(project)),
            root,
            size,
        })
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

        private::create_dir(&self.root)?;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn keeps_the_most_recently_used_analyses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("p"), 2);
        cache.put("1", "one").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        cache.put("2", "two").unwrap();
        std::thread::sleep(Duration::from_millis(20));

        assert_eq!(cache.get("1").as_deref(), Some("one"), "used again");

        std::thread::sleep(Duration::from_millis(20));
        cache.put("3", "three").unwrap();

        assert!(cache.contains("1"));
        assert!(!cache.contains("2"), "least recently used");
        assert!(cache.contains("3"));
        assert!(cache.remove("3").unwrap());
        assert!(!cache.remove("3").unwrap());
        assert_eq!(cache.get("3"), None);
    }

    #[test]
    fn caches_nothing_with_size_zero() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf(), 0);
        cache.put("1", "one").unwrap();

        assert!(!cache.contains("1"));
    }

    #[test]
    fn accepts_only_hub_ids() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().join("p"), 2);
        cache.put("../x", "outside").unwrap();
        cache.put("", "empty").unwrap();

        assert!(!dir.path().join("x.sarif").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn has_a_directory_per_hub_and_project() {
        let config = Config::from_value(&serde_json::json!({
            "hub": {"url": "http://hub:7340", "project": "my app"},
            "cacheDir": "/cache",
        }))
        .unwrap();

        let cache = Cache::of(&config).unwrap();

        assert_eq!(cache.dir, Path::new("/cache/http___hub_7340/my_app"));
        assert_eq!(cache.size, DEFAULT_SIZE);

        let config =
            Config::from_value(&serde_json::json!({"hub": {"url": "http://hub"}})).unwrap();
        assert!(Cache::of(&config).is_none(), "without a project");
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_analyses_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let cache = Cache {
            root: root.path().join("cache"),
            dir: root.path().join("cache/hub/project"),
            size: 2,
        };

        cache.put("1", "one").unwrap();

        let mode = std::fs::metadata(root.path().join("cache"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
