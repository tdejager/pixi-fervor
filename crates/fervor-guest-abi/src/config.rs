use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

/// What `fervor-init` runs once the layer stack is assembled.
///
/// Embedded in the image pack header, so it is per image, never per run:
/// run-time settings live in [`RunConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestConfig {
    /// Program and arguments; `argv[0]` is resolved against `PATH` from `env`.
    pub argv: Vec<String>,
    /// Complete environment of the entrypoint and every service, sorted by name.
    pub env: Vec<(String, String)>,
    /// Absolute working directory of the entrypoint and every service.
    pub workdir: String,
    /// Size limit of the tmpfs that backs the writable overlay layer.
    pub scratch_size_mib: u32,
    /// Long-running processes started, in order, before the entrypoint and
    /// restarted whenever they exit. They are stopped once the entrypoint is
    /// gone; the guest's exit status stays the entrypoint's.
    pub services: Vec<ServiceConfig>,
    /// Run `systemd-journald` before anything else and connect the stdout and
    /// stderr of every service and the entrypoint to it.
    pub journal: Option<JournalConfig>,
}

/// A supervised background process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceConfig {
    /// Unique among the image's services; the journal identifier of its output.
    pub name: String,
    /// Program and arguments; `argv[0]` is resolved like the entrypoint's.
    pub argv: Vec<String>,
}

/// The journal daemon of the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalConfig {
    /// Absolute path of the `systemd-journald` binary inside the guest.
    pub daemon: String,
}

/// Per-run settings, carried by the run pack on [`crate::RUN_DEVICE`]; the
/// run pack's layers are stacked on top of the image's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunConfig {
    /// `None`: the guest has no network interface besides loopback.
    pub network: Option<NetworkConfig>,
}

/// Static IPv4 configuration of the guest's `eth0`, a virtio-net device
/// backed by a TAP device on the run host that NATs the guest's traffic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub address: Ipv4Addr,
    /// Length of the network prefix shared with the gateway (e.g. 30).
    pub prefix_len: u8,
    /// The run host's end of the link; the guest's default route.
    pub gateway: Ipv4Addr,
    /// Complete contents of the guest's `/etc/resolv.conf`.
    pub resolv_conf: String,
}
