//! The JSON file passed to `firecracker --no-api --config-file`.

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::machine::MachineSpec;
use fervor_guest_abi::{CONTROL_PORT, GUEST_CID};
use serde::Serialize;

/// Firecracker appends `root=/dev/vda ro` for the read-only root drive.
/// The init pathname matches the boot layer's `fervor_domain::boot::INIT_PATH`.
const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 quiet rootfstype=squashfs init=/sbin/init";

/// Files of one boot inside its per-run directory.
#[derive(Debug, Clone)]
pub struct RunPaths {
    dir: Utf8PathBuf,
}

impl RunPaths {
    pub fn new(dir: impl Into<Utf8PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn config(&self) -> Utf8PathBuf {
        self.dir.join("firecracker.json")
    }

    pub fn log(&self) -> Utf8PathBuf {
        self.dir.join("firecracker.log")
    }

    /// Firecracker's vsock socket: host-initiated connections go here.
    pub fn vsock(&self) -> Utf8PathBuf {
        self.dir.join("vsock.sock")
    }

    /// Guest-initiated connections to `HOST_CID:CONTROL_PORT` land on this
    /// listener, which must exist before the guest connects.
    pub fn control_socket(&self) -> Utf8PathBuf {
        format!("{}_{CONTROL_PORT}", self.vsock()).into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FirecrackerConfig {
    #[serde(rename = "boot-source")]
    boot_source: BootSource,
    drives: Vec<Drive>,
    #[serde(rename = "machine-config")]
    machine_config: MachineConfig,
    vsock: Vsock,
    entropy: Entropy,
    logger: Logger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BootSource {
    kernel_image_path: Utf8PathBuf,
    boot_args: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Drive {
    drive_id: &'static str,
    path_on_host: Utf8PathBuf,
    is_root_device: bool,
    is_read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MachineConfig {
    vcpu_count: u8,
    mem_size_mib: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Vsock {
    guest_cid: u32,
    uds_path: Utf8PathBuf,
}

/// virtio-rng, so the guest's CSPRNG is seeded right away.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Entropy {}

const LOG_LEVEL: &str = "Warning";

/// Keeps Firecracker's own messages off the serial console (which is the
/// app's output). The log file must exist before Firecracker starts.
///
/// Firecracker logs its startup banner to stdout before it reads the config
/// file, so the level must also be given on the command line
/// ([`FirecrackerConfig::cli_args`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Logger {
    log_path: Utf8PathBuf,
    level: &'static str,
    show_level: bool,
    show_log_origin: bool,
}

impl FirecrackerConfig {
    /// The boot layer is the read-only root drive (`/dev/vda`), the pack the
    /// second read-only drive (`/dev/vdb`).
    pub fn new(kernel: &Utf8Path, boot_layer: &Utf8Path, pack: &Utf8Path, machine: &MachineSpec, paths: &RunPaths) -> Self {
        Self {
            boot_source: BootSource { kernel_image_path: kernel.to_owned(), boot_args: BOOT_ARGS },
            drives: vec![
                Drive { drive_id: "boot", path_on_host: boot_layer.to_owned(), is_root_device: true, is_read_only: true },
                Drive { drive_id: "pack", path_on_host: pack.to_owned(), is_root_device: false, is_read_only: true },
            ],
            machine_config: MachineConfig { vcpu_count: machine.vcpus.0.get(), mem_size_mib: machine.memory.0.get() },
            vsock: Vsock { guest_cid: GUEST_CID, uds_path: paths.vsock() },
            entropy: Entropy {},
            logger: Logger { log_path: paths.log(), level: LOG_LEVEL, show_level: true, show_log_origin: false },
        }
    }

    /// Arguments for `firecracker`, given the path this config is written to.
    pub fn cli_args(config_path: &Utf8Path) -> [&str; 5] {
        ["--no-api", "--config-file", config_path.as_str(), "--level", LOG_LEVEL]
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("firecracker config serializes")
    }
}

