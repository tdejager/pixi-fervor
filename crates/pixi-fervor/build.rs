//! pixi-fervor embeds the Linux binaries it ships into guests and run hosts,
//! so the extension is a single self-contained executable:
//! - `FERVOR_INIT_BIN`: fervor-init, PID 1 of every guest (always).
//! - `FERVOR_RUNNER_BIN`: fervor-runner, installed into the Lima run host
//!   (macOS builds only).
//!
//! `pixi run build` cross-compiles both and sets the variables.

use std::path::Path;

fn main() {
    embed("FERVOR_INIT_BIN", "fervor-init");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        embed("FERVOR_RUNNER_BIN", "fervor-runner");
    }
}

fn embed(var: &str, binary: &str) {
    println!("cargo:rerun-if-env-changed={var}");
    let path = std::env::var(var).unwrap_or_else(|_| {
        panic!("{var} must point at the cross-compiled {binary}; build with `pixi run build`")
    });
    assert!(Path::new(&path).is_file(), "{var}={path} does not exist; build with `pixi run build`");
    println!("cargo:rerun-if-changed={path}");
}
