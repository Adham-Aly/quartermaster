//! Creating the data directory with safe permissions.
//!
//! Every folder the app creates is mode 0700 and the root is tightened to 0700 on every start, so
//! nothing in it (briefs, `auth.json`) is readable by other users on the Mac. The scratch root
//! (crewmates' scratchpads) is checked here too; an unsafe hierarchy stops startup.

use qm_types::data_dir::DataDir;
use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

const DIR_MODE: u32 = 0o700;

/// Creates the data directory's fixed folders if they're missing (first run) and makes sure the
/// root is private. Per-agent and per-task folders are created when those exist.
pub fn prepare(dir: &DataDir) -> io::Result<()> {
    dir.ensure_scratch()?;
    let seat = dir.seat();
    for path in [
        dir.root(),
        dir.logs().as_path(),
        dir.workspace().as_path(),
        seat.root.as_path(),
        seat.home.as_path(),
        seat.session.as_path(),
        dir.secondmates_dir().as_path(),
        dir.tasks_dir().as_path(),
    ] {
        create_private_dir(path)?;
    }
    fs::set_permissions(dir.root(), Permissions::from_mode(DIR_MODE))?;
    Ok(())
}

/// `mkdir -p` with mode 0700 for every folder it creates.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_scratch_stops_startup_without_touching_link_target() {
        let base = std::env::temp_dir().join(format!("qm-home-scratch-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let victim = base.join("victim");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("unrelated"), b"safe").unwrap();
        let scratch = base.join("scratch");
        std::os::unix::fs::symlink(&victim, &scratch).unwrap();
        let data = DataDir::with_scratch(base.join("home"), scratch);
        assert!(prepare(&data).is_err());
        assert!(
            !data.root().exists(),
            "startup stopped before creating data files"
        );
        assert_eq!(fs::read(victim.join("unrelated")).unwrap(), b"safe");
        fs::remove_dir_all(base).unwrap();
    }
}
