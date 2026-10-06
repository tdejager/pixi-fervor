//! pixi-fervor embeds the Linux binaries it ships into guests and run hosts,
//! so the extension is a single self-contained executable:
//! - `FERVOR_INIT_BIN`: fervor-init, PID 1 of every guest (always).
//! - `FERVOR_RUNNER_BIN`: fervor-runner, installed into the Lima run host
//!   (macOS builds only).
//!
//! Both are cross-compiled here with `cargo zigbuild` (glibc 2.28) into a
//! target directory under `OUT_DIR`, so a plain `cargo build -p pixi-fervor`
//! (and `pixi build`) needs no separate step. `cargo-zigbuild` and `zig` must
//! be on `PATH`; the pixi environment provides them.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

const GUEST_TARGET: &str = "aarch64-unknown-linux-gnu";
const GUEST_GLIBC: &str = "2.28";

/// Workspace crates the guest binaries are built from; edits rebuild them.
const GUEST_CRATES: &[&str] = &[
    "fervor-init",
    "fervor-runner",
    "fervor-guest-abi",
    "fervor-domain",
    "fervor-store",
    "fervor-vmm",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/pixi-fervor -> workspace root
    let workspace = manifest_dir.parent().and_then(Path::parent).unwrap();
    let target_dir = PathBuf::from(env::var("OUT_DIR").unwrap()).join("guest-target");

    let mut binaries = vec![("FERVOR_INIT_BIN", "fervor-init")];
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        binaries.push(("FERVOR_RUNNER_BIN", "fervor-runner"));
    }

    let mut cargo = Command::new(env::var("CARGO").unwrap());
    cargo
        .current_dir(workspace)
        .args(["zigbuild", "--release", "--locked", "--target"])
        .arg(format!("{GUEST_TARGET}.{GUEST_GLIBC}"))
        .arg("--target-dir")
        .arg(&target_dir)
        // Flags cargo passes to this script are meant for the host build.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // This script's stdout carries cargo directives.
        .stdout(std::io::stderr());
    for (_, binary) in &binaries {
        cargo.args(["-p", binary]);
    }
    let status = cargo
        .status()
        .expect("failed to run `cargo zigbuild`; are cargo-zigbuild and zig on PATH?");
    assert!(status.success(), "cross-compiling the guest binaries failed: {status}");

    for (var, binary) in &binaries {
        let path = target_dir.join(GUEST_TARGET).join("release").join(binary);
        println!("cargo:rustc-env={var}={}", path.display());
    }

    for name in GUEST_CRATES {
        println!("cargo:rerun-if-changed={}", workspace.join("crates").join(name).display());
    }
    for file in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"] {
        println!("cargo:rerun-if-changed={}", workspace.join(file).display());
    }
}
