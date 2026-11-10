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
