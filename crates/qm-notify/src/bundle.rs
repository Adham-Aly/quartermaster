//! The helper's app bundle, `Quartermaster.app` in the data directory.
//!
//! macOS shows a notification only from a signed app that `LaunchServices` started, and draws the
//! app's icon on it. So the helper (embedded in this binary) is written out as a small menu bar
//! app, its icon drawn by the helper itself, and signed ad hoc. Signing rewrites the executable,
//! so a stamp of what was written tells whether the bundle is current: it is rewritten only when
//! this binary carries a different helper or plist.

use crate::NotifyError;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The helper, built from `helper/main.swift` by `build.rs`.
const HELPER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quartermaster-notifier"));

/// The executable's name inside the bundle.
const EXECUTABLE: &str = "quartermaster-notifier";

/// The bundle's identity: the real data directory's app and a development one are told apart, so
/// a throwaway run never shares your real app's notification settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Identity {
    Real,
    Development,
}

impl Identity {
    /// The real app lives in `~/.quartermaster`.
    pub(crate) fn of(root: &Path) -> Self {
        let real = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".quartermaster"));
        if real.as_deref() == Some(root) {
            Self::Real
        } else {
            Self::Development
        }
    }

    const fn bundle_id(self) -> &'static str {
        match self {
            Self::Real => "io.github.flotilla-labs.quartermaster",
            Self::Development => "io.github.flotilla-labs.quartermaster.dev",
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Real => "Quartermaster",
            Self::Development => "Quartermaster Dev",
        }
    }
}

fn info_plist(identity: Identity) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>{id}</string>
	<key>CFBundleName</key>
	<string>{name}</string>
	<key>CFBundleDisplayName</key>
	<string>{name}</string>
	<key>CFBundleExecutable</key>
	<string>{EXECUTABLE}</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSMinimumSystemVersion</key>
	<string>14.0</string>
	<key>LSUIElement</key>
	<true/>
</dict>
</plist>
"#,
        id = identity.bundle_id(),
        name = identity.name(),
        version = env!("CARGO_PKG_VERSION"),
    )
}

/// Makes sure the bundle at `app` holds this binary's helper, signed. Blocking.
pub(crate) fn install(app: &Path, identity: Identity) -> Result<(), NotifyError> {
    let contents = app.join("Contents");
    let plist_path = contents.join("Info.plist");
    let executable = contents.join("MacOS").join(EXECUTABLE);
    let icon = contents.join("Resources").join("AppIcon.icns");
    let signature = contents.join("_CodeSignature");
    let stamp_path = contents.join("Resources").join("stamp");
    let plist = info_plist(identity);
    let stamp = stamp(&plist);
    let current = fs::read_to_string(&stamp_path).is_ok_and(|s| s == stamp)
        && fs::read_to_string(&plist_path).is_ok_and(|p| p == plist)
        && executable.is_file()
        && icon.is_file()
        && signature.is_dir();
    if current {
        return Ok(());
    }
    let failed = |source: io::Error| NotifyError::Install {
        path: app.to_owned(),
        source,
    };
    if app.exists() {
        fs::remove_dir_all(app).map_err(failed)?;
    }
    for dir in ["MacOS", "Resources"] {
        fs::create_dir_all(contents.join(dir)).map_err(failed)?;
    }
    fs::write(&plist_path, plist).map_err(failed)?;
    fs::write(&executable, HELPER).map_err(failed)?;
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).map_err(failed)?;
    run(
        Command::new(&executable).arg("--make-icon").arg(&icon),
        NotifyError::Icon,
    )?;
    // Written before signing, so the signature seals it with the rest.
    fs::write(&stamp_path, stamp).map_err(failed)?;
    run(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(app),
        NotifyError::Sign,
    )?;
    tracing::info!(app = %app.display(), "installed the notifier app");
    Ok(())
}

/// What this binary writes into the bundle, as a hash (FNV-1a, stable across builds).
fn stamp(plist: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in HELPER.iter().chain(plist.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}\n")
}

/// Runs `command`, its failure (and what it printed) becoming `error`.
fn run(command: &mut Command, error: fn(String) -> NotifyError) -> Result<(), NotifyError> {
    let output = command.output().map_err(|e| error(e.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    Err(error(format!("{} {}", output.status, said.trim())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_names_the_executable_and_stays_out_of_the_dock() {
        let plist = info_plist(Identity::Development);
        assert!(plist.contains("<string>io.github.flotilla-labs.quartermaster.dev</string>"));
        assert!(plist.contains("<string>quartermaster-notifier</string>"));
        assert!(plist.contains("<key>LSUIElement</key>\n\t<true/>"));
    }

    #[test]
    fn the_stamp_follows_the_plist() {
        let real = stamp(&info_plist(Identity::Real));
        assert_eq!(real, stamp(&info_plist(Identity::Real)));
        assert_ne!(real, stamp(&info_plist(Identity::Development)));
    }

    #[test]
    fn a_signed_install_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("qm-notify-test-{}", std::process::id()));
        let app = dir.join("Quartermaster.app");
        install(&app, Identity::Development).unwrap();
        let signed = fs::metadata(app.join("Contents/_CodeSignature"))
            .unwrap()
            .modified()
            .unwrap();
        install(&app, Identity::Development).unwrap();
        let again = fs::metadata(app.join("Contents/_CodeSignature"))
            .unwrap()
            .modified()
            .unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(signed, again);
    }

    #[test]
    fn only_the_home_data_directory_is_the_real_app() {
        assert_eq!(
            Identity::of(Path::new("/nowhere/.quartermaster")),
            Identity::Development
        );
        if let Some(home) = std::env::var_os("HOME") {
            let real = PathBuf::from(home).join(".quartermaster");
            assert_eq!(Identity::of(&real), Identity::Real);
        }
    }
}
