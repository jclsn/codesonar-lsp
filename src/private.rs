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
