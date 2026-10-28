//! `fervor-init`: PID 1 of a fervor guest.
//!
//! Mounts the pseudo filesystems, stacks the layers of the image and run pack
//! devices into an overlay root, pivots into it, configures the network, starts
//! the journal and the image's services, runs the image entrypoint and reports
//! its fate to the host over vsock before rebooting (which ends the Firecracker
//! VM).

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod plan;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    linux::Init::run()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("fervor-init: only runs as PID 1 inside a Linux fervor guest");
    std::process::ExitCode::FAILURE
}
