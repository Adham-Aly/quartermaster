//! Compiles the notifier helper (`helper/main.swift`) for the target's architecture with the
//! Xcode command line tools' `swiftc`; the library embeds the result.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=helper/main.swift");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let target = env::var("TARGET").expect("cargo sets TARGET");
    let arch = match target.split('-').next() {
        Some("aarch64") => "arm64",
        Some("x86_64") => "x86_64",
        _ => panic!("the notifier helper is built only for macOS on arm64 or x86_64, not {target}"),
    };
    let binary = out.join("quartermaster-notifier");
    let status = Command::new("xcrun")
        .args(["swiftc", "-O", "-target"])
        .arg(format!("{arch}-apple-macos14"))
        .arg("helper/main.swift")
        .arg("-o")
        .arg(&binary)
        .status()
        .unwrap_or_else(|e| {
            panic!("can't run xcrun swiftc ({e}); install the Xcode command line tools")
        });
    assert!(
        status.success(),
        "swiftc couldn't build the notifier helper"
    );
}
