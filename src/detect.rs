//! Detects the CodeSonar project of a workspace from the files that
//! `codesonar analyze` leaves there.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The project files of a workspace, e.g. `mixxx.prj_files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProject {
    /// The project prefix, e.g. `/ws/mixxx`.
    pub prefix: PathBuf,
    /// The hub id of the last analysis of the project.
    pub analysis_id: Option<String>,
}

const SUFFIX: &str = ".prj_files";

/// The project files in the workspace root or in `.codesonar`, the most
/// recently analyzed if there are several.
pub fn local_project(root: &Path) -> Option<LocalProject> {
    [root.to_path_buf(), root.join(".codesonar")]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let prefix = path.with_file_name(name.strip_suffix(SUFFIX)?);
            path.is_dir().then_some((modified(&path), prefix, path))
        })
        .max_by_key(|(modified, _, _)| *modified)
        .map(|(_, prefix, files)| LocalProject {
            prefix,
            analysis_id: std::fs::read_to_string(files.join("aid.txt"))
                .ok()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
        })
}

fn modified(path: &Path) -> SystemTime {
    // The analysis id is rewritten by every analysis.
    std::fs::metadata(path.join("aid.txt"))
        .or_else(|_| std::fs::metadata(path))
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn project_files(dir: &Path, name: &str, analysis_id: &str) -> PathBuf {
        let files = dir.join(format!("{name}{SUFFIX}"));
        std::fs::create_dir_all(&files).unwrap();
        std::fs::write(files.join("aid.txt"), format!("{analysis_id}\n")).unwrap();
        files
    }

    #[test]
    fn finds_project_files_in_the_root() {
        let root = tempfile::tempdir().unwrap();
        project_files(root.path(), "mixxx", "8");
        std::fs::write(root.path().join("mixxx.prj"), "").unwrap();

        assert_eq!(
            local_project(root.path()),
            Some(LocalProject {
                prefix: root.path().join("mixxx"),
                analysis_id: Some("8".into()),
            })
        );
    }

    #[test]
    fn finds_project_files_in_the_codesonar_directory() {
        let root = tempfile::tempdir().unwrap();
        let files = project_files(&root.path().join(".codesonar"), "project", "");
        std::fs::remove_file(files.join("aid.txt")).unwrap();

        assert_eq!(
            local_project(root.path()),
            Some(LocalProject {
                prefix: root.path().join(".codesonar/project"),
                analysis_id: None,
            })
        );
    }

    #[test]
    fn prefers_the_last_analyzed_project() {
        let root = tempfile::tempdir().unwrap();
        project_files(root.path(), "old", "1");
        std::thread::sleep(Duration::from_millis(20));
        project_files(root.path(), "new", "2");
        let project = local_project(root.path()).unwrap();

        assert_eq!(project.prefix, root.path().join("new"));
    }

    #[test]
    fn finds_nothing_without_project_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("other.prj_files"), "").unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();

        assert_eq!(local_project(root.path()), None);
    }
}
