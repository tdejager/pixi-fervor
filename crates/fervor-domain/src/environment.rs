//! Environment context: which exact packages make up a guest.

use std::str::FromStr;

use rattler_conda_types::{
    Channel, GenericVirtualPackage, MatchSpec, PackageName, PackageRecord, RepoDataRecord,
    Version,
};

use crate::digest::PackageSha256;
use crate::nonempty::NonEmpty;
use crate::platform::{GuestLibc, GuestPlatform};
use crate::size::ByteSize;

/// A complete, resolvable description of a guest environment.
#[derive(Debug, Clone)]
pub struct EnvironmentSpec {
    requirements: NonEmpty<MatchSpec>,
    /// Channels in priority order.
    channels: NonEmpty<Channel>,
    platform: GuestPlatform,
    libc: GuestLibc,
    /// Version of the guest kernel, exposed as the `__linux` virtual package.
    linux: Version,
}

impl EnvironmentSpec {
    pub fn new(
        requirements: NonEmpty<MatchSpec>,
        channels: NonEmpty<Channel>,
        platform: GuestPlatform,
        libc: GuestLibc,
        linux: Version,
    ) -> Self {
        Self { requirements, channels, platform, libc, linux }
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    pub fn platform(&self) -> GuestPlatform {
        self.platform
    }

    pub fn libc(&self) -> &GuestLibc {
        &self.libc
    }

    /// Virtual packages of the guest: what the solver may assume exists
    /// without installing it.
    pub fn virtual_packages(&self) -> Vec<GenericVirtualPackage> {
        let virtual_package = |name: &str, version: Version, build: &str| GenericVirtualPackage {
            name: PackageName::new_unchecked(name),
            version,
            build_string: build.to_owned(),
        };
        let zero = Version::from_str("0").expect("valid version");
        vec![
            virtual_package("__unix", zero.clone(), "0"),
            virtual_package("__linux", self.linux.clone(), "0"),
            self.libc.virtual_package(),
            virtual_package("__archspec", Version::from_str("1").expect("valid"), self.platform.archspec()),
        ]
    }

    /// The user's requirements plus the sysroot that provides the guest libc.
    pub fn solver_requirements(&self) -> Vec<MatchSpec> {
        let mut specs = self.requirements.to_vec();
        specs.push(self.libc.sysroot_spec(self.platform));
        specs
    }
}

/// Python version of an environment; decides where `noarch: python`
/// packages land (`lib/pythonX.Y/site-packages`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct PythonAbi {
    pub major: u64,
    pub minor: u64,
}

impl PythonAbi {
    pub fn site_packages(self) -> String {
        format!("lib/python{}.{}/site-packages", self.major, self.minor)
    }

    pub fn interpreter(self) -> String {
        format!("bin/python{}.{}", self.major, self.minor)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("package `{0}` has no sha256 in its repodata; layer keys need one")]
    MissingSha256(String),
    #[error("package `{0}` has no size in its repodata; layer planning needs one")]
    MissingSize(String),
    #[error("the solution does not contain `{0}`, which provides the guest libc")]
    MissingSysroot(String),
    #[error("python `{0}` has no major.minor version")]
    UnparsablePython(String),
}

/// One package of a resolved environment. Invariant: sha256 and size known.
#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    record: RepoDataRecord,
    sha256: PackageSha256,
    archive_size: ByteSize,
}

impl ResolvedPackage {
    pub fn new(record: RepoDataRecord) -> Result<Self, EnvironmentError> {
        let name = || record.package_record.name.as_normalized().to_owned();
        let sha256 = record
            .package_record
            .sha256
            .map(PackageSha256::from_hash)
            .ok_or_else(|| EnvironmentError::MissingSha256(name()))?;
        let archive_size = record
            .package_record
            .size
            .map(ByteSize::bytes)
            .ok_or_else(|| EnvironmentError::MissingSize(name()))?;
        Ok(Self { record, sha256, archive_size })
    }

    pub fn record(&self) -> &RepoDataRecord {
        &self.record
    }

    pub fn name(&self) -> &PackageName {
        &self.record.package_record.name
    }

    /// `name-version-build`, the conventional display form.
    pub fn display_name(&self) -> String {
        let record = &self.record.package_record;
        format!("{}-{}-{}", record.name.as_normalized(), record.version, record.build)
    }

    pub fn sha256(&self) -> PackageSha256 {
        self.sha256
    }

    pub fn archive_size(&self) -> ByteSize {
        self.archive_size
    }

    pub fn is_noarch_python(&self) -> bool {
        self.record.package_record.noarch.is_python()
    }
}

impl AsRef<PackageRecord> for ResolvedPackage {
    fn as_ref(&self) -> &PackageRecord {
        &self.record.package_record
    }
}

/// The exact package set of a guest.
///
/// Invariants: `packages` is in topological install order (dependencies
/// first) and excludes the sysroot, which is split out because it feeds the
/// boot layer instead of the environment prefix.
#[derive(Debug, Clone)]
pub struct ResolvedEnvironment {
    platform: GuestPlatform,
    sysroot: ResolvedPackage,
    packages: Vec<ResolvedPackage>,
    python: Option<PythonAbi>,
}

impl ResolvedEnvironment {
    pub fn new(
        platform: GuestPlatform,
        packages: Vec<ResolvedPackage>,
    ) -> Result<Self, EnvironmentError> {
        let sysroot_name = platform.sysroot_package();
        let mut packages = PackageRecord::sort_topologically(packages);
        let sysroot_index = packages
            .iter()
            .position(|p| p.name() == &sysroot_name)
            .ok_or_else(|| EnvironmentError::MissingSysroot(sysroot_name.as_normalized().to_owned()))?;
        let sysroot = packages.remove(sysroot_index);

        let python = packages
            .iter()
            .find(|p| p.name().as_normalized() == "python")
            .map(|p| {
                let version = &p.record().package_record.version;
                version
                    .as_major_minor()
                    .map(|(major, minor)| PythonAbi { major, minor })
                    .ok_or_else(|| EnvironmentError::UnparsablePython(version.to_string()))
            })
            .transpose()?;

        Ok(Self { platform, sysroot, packages, python })
    }

    pub fn platform(&self) -> GuestPlatform {
        self.platform
    }

    pub fn sysroot(&self) -> &ResolvedPackage {
        &self.sysroot
    }

    /// Packages installed into the environment prefix, in install order.
    pub fn packages(&self) -> &[ResolvedPackage] {
        &self.packages
    }

    pub fn python(&self) -> Option<PythonAbi> {
        self.python
    }
}
