//! Directories only the user can read, for the downloaded analyses and the
//! state, which contain source code and warnings.

use std::path::Path;

use anyhow::Context;

/// Creates `dir` with its parents, readable only by the user. An existing
/// directory gets these permissions too.
pub fn create_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("cannot protect {}", dir.display()))?;
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(dir: &Path) -> u32 {
        std::fs::metadata(dir).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn creates_directories_only_the_user_can_read() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a/b");

        create_dir(&dir).unwrap();

        assert_eq!(mode(&dir), 0o700);

        // An existing directory is protected as well.
        let open = root.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();

        create_dir(&open).unwrap();

        assert_eq!(mode(&open), 0o700);
    }
}
