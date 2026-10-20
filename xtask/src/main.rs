//! Repository automation, run as `cargo xtask <task>`.
//!
//! - `build`: build every crate and target (debug).
//! - `check`: format check and clippy with warnings as errors.
//! - `ci`: exactly what CI runs: format check, clippy with warnings as errors, then the build.
//! - `install`: build the release binary and install `quartermaster` into the `bin` folder of
//!   `CARGO_INSTALL_ROOT`, `CARGO_HOME` or `~/.cargo`, and print where it went.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const TASKS: &[(&str, &str)] = &[
    ("build", "build every crate and target"),
    ("check", "format check and clippy with warnings as errors"),
    (
        "ci",
        "run exactly what CI runs: fmt, clippy -D warnings, the build",
    ),
    (
        "install",
        "build the release binary and install quartermaster into $CARGO_HOME/bin (~/.cargo/bin)",
    ),
];

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let steps: &[&[&str]] = match task.as_deref() {
        Some("build") => &[BUILD],
        Some("check") => &[FMT_CHECK, CLIPPY],
        Some("ci") => &[FMT_CHECK, CLIPPY, BUILD],
        Some("install") => &[INSTALL],
        other => {
            eprintln!("xtask: unknown task {}. Tasks:", other.unwrap_or("(none)"));
            for (name, what) in TASKS {
                eprintln!("  {name:<8} {what}");
            }
            return ExitCode::FAILURE;
        }
    };
    let root = workspace_root();
    for args in steps {
        eprintln!("xtask: cargo {}", args.join(" "));
        let mut cmd = Command::new(cargo());
        cmd.args(*args).current_dir(&root);
        if *args == INSTALL {
            cmd.arg("--root").arg(install_root());
        }
        match cmd.status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!("xtask: cargo {} failed ({status})", args.join(" "));
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("xtask: couldn't run cargo: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    if task.as_deref() == Some("install") {
        return report_install();
    }
    ExitCode::SUCCESS
}

const BUILD: &[&str] = &["build", "--workspace", "--all-targets", "--locked"];
const FMT_CHECK: &[&str] = &["fmt", "--all", "--check"];
const CLIPPY: &[&str] = &[
    "clippy",
    "--workspace",
    "--all-targets",
    "--locked",
    "--",
    "-D",
    "warnings",
];
const INSTALL: &[&str] = &[
    "install",
    "--path",
    "crates/quartermaster",
    "--locked",
    "--force",
    "--target-dir",
    "target",
];

/// Where `cargo install` puts the binary's `bin` folder: `CARGO_INSTALL_ROOT`, else
/// `CARGO_HOME`, else `~/.cargo`. Passed as `--root`, so the destination printed is
/// the one used.
fn install_root() -> PathBuf {
    let var = |key: &str| {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    var("CARGO_INSTALL_ROOT")
        .or_else(|| var("CARGO_HOME"))
        .or_else(|| var("HOME").map(|home| home.join(".cargo")))
        .unwrap_or_else(|| PathBuf::from(".cargo"))
}

/// After `cargo install`: checks the binary is there, says where, and whether that folder is on
/// `PATH`.
fn report_install() -> ExitCode {
    let bin = install_root().join("bin");
    let exe = bin.join("quartermaster");
    if !exe.is_file() {
        eprintln!(
            "xtask: cargo install finished but {} isn't there",
            exe.display()
        );
        return ExitCode::FAILURE;
    }
    eprintln!("xtask: installed quartermaster at {}", exe.display());
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir == bin));
    if !on_path {
        eprintln!(
            "xtask: {} isn't on your PATH; add it (see README.md, \"Install\")",
            bin.display()
        );
    }
    ExitCode::SUCCESS
}

/// The cargo that ran us, so the toolchain stays the same.
fn cargo() -> OsString {
    std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// The checkout this xtask was built from.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits one level below the workspace root")
        .to_path_buf()
}
