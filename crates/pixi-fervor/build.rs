//! pixi-fervor embeds the Linux binaries it ships into guests and run hosts,
//! so the extension is a single self-contained executable:
//! - `FERVOR_INIT_AARCH64_BIN`, `FERVOR_INIT_X86_64_BIN`: fervor-init, PID 1
//!   of every guest; the image's platform picks one.
//! - `FERVOR_RUNNER_BIN`: fervor-runner, installed into the Lima run host
//!   (macOS builds only; the VM has the Mac's architecture).
//!
//! They are cross-compiled here with `cargo zigbuild` (glibc 2.28) into a
//! target directory under `OUT_DIR`, so a plain `cargo build -p pixi-fervor`
//! (and `pixi build`) needs no separate step. `cargo-zigbuild`, `zig` and the
//! Rust std for each Linux target must be available; the pixi environment
//! provides them.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

const GUEST_GLIBC: &str = "2.28";

/// fervor-init for every guest architecture: (arch, embedding variable).
const INIT_ARCHES: &[(&str, &str)] = &[("aarch64", "FERVOR_INIT_AARCH64_BIN"), ("x86_64", "FERVOR_INIT_X86_64_BIN")];

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
    let guest = GuestBuild { workspace, target_dir: PathBuf::from(env::var("OUT_DIR").unwrap()).join("guest-target") };

    for (arch, var) in INIT_ARCHES {
        guest.embed(var, "fervor-init", arch);
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        guest.embed("FERVOR_RUNNER_BIN", "fervor-runner", &env::var("CARGO_CFG_TARGET_ARCH").unwrap());
    }

    for name in GUEST_CRATES {
        println!("cargo:rerun-if-changed={}", workspace.join("crates").join(name).display());
    }
    for file in ["Cargo.toml", "Cargo.lock", ".cargo/config.toml"] {
        println!("cargo:rerun-if-changed={}", workspace.join(file).display());
    }
}

struct GuestBuild<'a> {
    workspace: &'a Path,
    target_dir: PathBuf,
}

impl GuestBuild<'_> {
    /// Cross-compiles `package` for Linux on `arch` and points `var` at it.
    fn embed(&self, var: &str, package: &str, arch: &str) {
        let target = format!("{arch}-unknown-linux-gnu");
        let status = Command::new(env::var("CARGO").unwrap())
            .current_dir(self.workspace)
            .args(["zigbuild", "--release", "--locked", "-p", package, "--target"])
            .arg(format!("{target}.{GUEST_GLIBC}"))
            .arg("--target-dir")
            .arg(&self.target_dir)
            // Flags cargo passes to this script are meant for the host build.
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            // conda's rust sets this on Linux when `target` is the host; gcc
            // would then link the guest against a shared libgcc_s, which the
            // boot layer lacks. zig links the unwinder statically.
            .env_remove(format!("CARGO_TARGET_{}_LINKER", target.to_uppercase().replace('-', "_")))
            // This script's stdout carries cargo directives.
            .stdout(std::io::stderr())
            .status()
            .expect("failed to run `cargo zigbuild`; are cargo-zigbuild and zig on PATH?");
        assert!(status.success(), "cross-compiling {package} for {target} failed: {status}");
        let path = self.target_dir.join(&target).join("release").join(package);
        println!("cargo:rustc-env={var}={}", path.display());
    }
}
