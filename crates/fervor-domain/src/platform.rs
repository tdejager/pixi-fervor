use std::fmt;
use std::str::FromStr;

use rattler_conda_types::{
    GenericVirtualPackage, MatchSpec, PackageName, ParseStrictness, Subdir, Version,
};
use serde::{Deserialize, Serialize};

use crate::path::GuestPath;

/// A platform Firecracker can boot. Deliberately narrower than conda's
/// `Subdir`: only Linux on the architectures Firecracker supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub enum GuestPlatform {
    LinuxAarch64,
    LinuxX86_64,
}

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not a guest platform (supported: linux-aarch64, linux-64)")]
pub struct UnsupportedPlatform(pub String);

/// Image format the kernel must be in for Firecracker on this architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelFormat {
    /// Uncompressed ELF (`vmlinux`), x86_64.
    Vmlinux,
    /// PE formatted `Image`, aarch64.
    PeImage,
}

impl GuestPlatform {
    pub fn subdir(self) -> Subdir {
        match self {
            Self::LinuxAarch64 => Subdir::LinuxAarch64,
            Self::LinuxX86_64 => Subdir::Linux64,
        }
    }

    pub fn from_subdir(subdir: Subdir) -> Result<Self, UnsupportedPlatform> {
        match subdir {
            Subdir::LinuxAarch64 => Ok(Self::LinuxAarch64),
            Subdir::Linux64 => Ok(Self::LinuxX86_64),
            other => Err(UnsupportedPlatform(other.to_string())),
        }
    }

    /// `__archspec` value used when solving.
    pub fn archspec(self) -> &'static str {
        match self {
            Self::LinuxAarch64 => "aarch64",
            Self::LinuxX86_64 => "x86_64",
        }
    }

    /// conda-forge package that provides glibc for this platform.
    pub fn sysroot_package(self) -> PackageName {
        let name = match self {
            Self::LinuxAarch64 => "sysroot_linux-aarch64",
            Self::LinuxX86_64 => "sysroot_linux-64",
        };
        PackageName::new_unchecked(name)
    }

    /// Directory inside the sysroot package that mirrors a Linux root.
    pub fn sysroot_root(self) -> &'static str {
        match self {
            Self::LinuxAarch64 => "aarch64-conda-linux-gnu/sysroot",
            Self::LinuxX86_64 => "x86_64-conda-linux-gnu/sysroot",
        }
    }

    /// Program interpreter compiled into dynamically linked binaries.
    pub fn loader_path(self) -> GuestPath {
        let path = match self {
            Self::LinuxAarch64 => "/lib/ld-linux-aarch64.so.1",
            Self::LinuxX86_64 => "/lib64/ld-linux-x86-64.so.2",
        };
        GuestPath::new(path).expect("static path is valid")
    }

    pub fn kernel_format(self) -> KernelFormat {
        match self {
            Self::LinuxAarch64 => KernelFormat::PeImage,
            Self::LinuxX86_64 => KernelFormat::Vmlinux,
        }
    }
}

impl fmt::Display for GuestPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.subdir())
    }
}

impl FromStr for GuestPlatform {
    type Err = UnsupportedPlatform;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let subdir = Subdir::from_str(s).map_err(|_| UnsupportedPlatform(s.to_owned()))?;
        Self::from_subdir(subdir)
    }
}

impl From<GuestPlatform> for String {
    fn from(platform: GuestPlatform) -> Self {
        platform.to_string()
    }
}

impl TryFrom<String> for GuestPlatform {
    type Error = UnsupportedPlatform;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// The glibc the guest runs on. Single source of truth for both the
/// `__glibc` virtual package and the sysroot package that provides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestLibc {
    version: Version,
}

impl GuestLibc {
    pub fn new(version: Version) -> Self {
        Self { version }
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn virtual_package(&self) -> GenericVirtualPackage {
        GenericVirtualPackage {
            name: PackageName::new_unchecked("__glibc"),
            version: self.version.clone(),
            build_string: "0".to_owned(),
        }
    }

    /// Requirement that pulls in exactly this glibc's sysroot.
    pub fn sysroot_spec(&self, platform: GuestPlatform) -> MatchSpec {
        let spec = format!("{} =={}", platform.sysroot_package().as_normalized(), self.version);
        MatchSpec::from_str(&spec, ParseStrictness::Strict).expect("valid sysroot match spec")
    }
}

impl Default for GuestLibc {
    /// glibc 2.39: the newest conda-forge sysroot.
    fn default() -> Self {
        Self::new(Version::from_str("2.39").expect("valid version"))
    }
}
