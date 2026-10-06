//! Machine context: how an image is booted and what came out of it.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::num::{NonZeroU8, NonZeroU32};
use std::str::FromStr;
use std::time::Duration;

use rattler_conda_types::Version;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::digest::ArtifactDigest;
use crate::platform::KernelFormat;

/// A file downloaded from `url`, optionally extracted from a `.tgz` at
/// `member`, whose bytes must hash to `sha256`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedArtifact {
    pub url: Url,
    /// Path inside a gzip'd tarball; `None` when `url` is the file itself.
    pub member: Option<String>,
    pub sha256: ArtifactDigest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelArtifact {
    pub artifact: PinnedArtifact,
    pub format: KernelFormat,
    /// Exposed to the solver as the `__linux` virtual package.
    pub version: String,
}

/// Hypervisor and guest kernel. Firecracker's CI publishes kernels per
/// release, so both are pinned — and upgraded — together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSet {
    pub firecracker: PinnedArtifact,
    pub kernel: KernelArtifact,
}

impl ArtifactSet {
    pub fn kernel_version(&self) -> Version {
        Version::from_str(&self.kernel.version).expect("pinned kernel version is valid")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VcpuCount(pub NonZeroU8);

impl Default for VcpuCount {
    fn default() -> Self {
        Self(NonZeroU8::new(2).expect("non-zero"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryMib(pub NonZeroU32);

impl Default for MemoryMib {
    fn default() -> Self {
        Self(NonZeroU32::new(512).expect("non-zero"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineSpec {
    pub vcpus: VcpuCount,
    pub memory: MemoryMib,
}

impl MachineSpec {
    pub fn new(vcpus: VcpuCount, memory: MemoryMib) -> Self {
        Self { vcpus, memory }
    }
}

/// `-p 8080:5000` / `-p 0.0.0.0:8080:5000`: a host TCP address forwarded to a
/// guest TCP port on `127.0.0.1` over vsock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    pub host: SocketAddr,
    pub guest_port: u16,
}

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not a port forward (expected `HOST_PORT:GUEST_PORT` or `HOST_IP:HOST_PORT:GUEST_PORT`)")]
pub struct PortForwardParseError(String);

impl FromStr for PortForward {
    type Err = PortForwardParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || PortForwardParseError(s.to_owned());
        let (host, guest) = s.rsplit_once(':').ok_or_else(err)?;
        let guest_port: u16 = guest.parse().map_err(|_| err())?;
        let host = match host.parse::<u16>() {
            Ok(port) => SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)),
            Err(_) => host.parse().map_err(|_| err())?,
        };
        Ok(Self { host, guest_port })
    }
}

impl fmt::Display for PortForward {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.guest_port)
    }
}

/// Everything about one boot that is not part of the image.
#[derive(Debug, Clone)]
pub struct RunRequest {
    pub machine: MachineSpec,
    pub forwards: Vec<PortForward>,
    /// Time between SIGTERM and SIGKILL when the host asks for shutdown.
    pub shutdown_grace: Duration,
}

/// How the guest's entrypoint ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestExit {
    Exited(i32),
    Signaled(i32),
    InitFailed(String),
    /// The VM stopped without the guest reporting (crash, kill, panic).
    NoReport,
}

impl GuestExit {
    /// Exit status for the host process, following shell conventions.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Exited(code) => *code,
            Self::Signaled(signal) => 128 + signal,
            Self::InitFailed(_) | Self::NoReport => 125,
        }
    }
}

impl fmt::Display for GuestExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exited(code) => write!(f, "exited with code {code}"),
            Self::Signaled(signal) => write!(f, "was killed by signal {signal}"),
            Self::InitFailed(message) => write!(f, "failed to start: {message}"),
            Self::NoReport => f.write_str("stopped without reporting an exit status"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub exit: GuestExit,
    pub wall_time: Duration,
}

impl fmt::Display for RunOutcome {
    /// `guest exited with code 0 after 1.6s`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "guest {} after {:.1}s", self.exit, self.wall_time.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_forward_defaults_to_loopback() {
        let fwd: PortForward = "8080:5000".parse().unwrap();
        assert_eq!(fwd.host, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(fwd.guest_port, 5000);
        let any: PortForward = "0.0.0.0:80:5000".parse().unwrap();
        assert_eq!(any.host, "0.0.0.0:80".parse().unwrap());
        assert!("8080".parse::<PortForward>().is_err());
        assert!("8080:99999".parse::<PortForward>().is_err());
    }

    #[test]
    fn exit_codes_follow_shell_conventions() {
        assert_eq!(GuestExit::Exited(3).exit_code(), 3);
        assert_eq!(GuestExit::Signaled(15).exit_code(), 143);
        assert_eq!(GuestExit::NoReport.exit_code(), 125);
    }
}
