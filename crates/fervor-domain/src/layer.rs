//! Layering context: how packages and host files are grouped into
//! content-addressed layers.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};

use crate::digest::{ArtifactDigest, CanonicalHasher, LayerDigest, LayerKey, PackageSha256, TreeDigest};
use crate::environment::{PythonAbi, ResolvedEnvironment, ResolvedPackage};
use crate::files::{FileOwner, FileSet};
use crate::nonempty::NonEmpty;
use crate::path::GuestPath;
use crate::platform::GuestPlatform;
use crate::size::ByteSize;

/// Where the environment is installed inside the guest. Prefix placeholders
/// are replaced with this path, so it is part of every package layer key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvPrefix(GuestPath);

impl EnvPrefix {
    pub fn new(path: GuestPath) -> Self {
        Self(path)
    }

    pub fn path(&self) -> &GuestPath {
        &self.0
    }
}

impl Default for EnvPrefix {
    fn default() -> Self {
        Self(GuestPath::new("/opt/env").expect("valid path"))
    }
}

/// Version of the layer format (file attributes, compression, builder
/// version). Bumping it invalidates every cached layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutVersion(pub u16);

impl LayoutVersion {
    pub const CURRENT: Self = Self(1);
}

/// What makes one package's installed files deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemberKey {
    pub package: PackageSha256,
    /// Only for `noarch: python` packages: their install location depends on it.
    pub python: Option<PythonAbi>,
}

impl MemberKey {
    pub fn of(package: &ResolvedPackage, python: Option<PythonAbi>) -> Self {
        Self {
            package: package.sha256(),
            python: package.is_noarch_python().then_some(python).flatten(),
        }
    }
}

/// Everything a layer key is derived from.
pub enum LayerKeyInput<'a> {
    Packages { prefix: &'a EnvPrefix, members: &'a [MemberKey] },
    HostTree { mount_at: &'a GuestPath, tree: TreeDigest },
    Boot { platform: GuestPlatform, sysroot: PackageSha256, init: ArtifactDigest },
}

impl LayerKey {
    /// Canonical derivation. Members of a package group are sorted first, so
    /// the key does not depend on the order packages were planned in.
    pub fn derive(layout: LayoutVersion, input: LayerKeyInput<'_>) -> Self {
        let mut hasher = CanonicalHasher::new("fervor/layer-key");
        hasher.u64(layout.0.into());
        match input {
            LayerKeyInput::Packages { prefix, members } => {
                let mut members = members.to_vec();
                members.sort();
                hasher.str("packages").str(prefix.path().as_str()).u64(members.len() as u64);
                for member in members {
                    hasher.bytes(&member.package.to_bytes());
                    match member.python {
                        Some(python) => hasher.u64(1).u64(python.major).u64(python.minor),
                        None => hasher.u64(0),
                    };
                }
            }
            LayerKeyInput::HostTree { mount_at, tree } => {
                hasher.str("host-tree").str(mount_at.as_str()).bytes(&tree.to_bytes());
            }
            LayerKeyInput::Boot { platform, sysroot, init } => {
                hasher
                    .str("boot")
                    .str(&platform.to_string())
                    .bytes(&sysroot.to_bytes())
                    .bytes(&init.to_bytes());
            }
        }
        Self::from_hash(hasher.finish())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayerKind {
    /// Bootable root: init, glibc and the `/etc` skeleton.
    Boot,
    /// A single package.
    Package,
    /// Several small packages merged into one layer.
    SmallGroup,
    /// A directory copied from the host.
    HostTree,
}

/// A host directory, scanned and hashed, ready to become a layer.
#[derive(Debug, Clone)]
pub struct HostTreeSnapshot {
    pub source: Utf8PathBuf,
    pub mount_at: GuestPath,
    pub digest: TreeDigest,
    pub files: FileSet,
}

/// Where a planned layer's files come from.
#[derive(Debug, Clone)]
pub enum LayerSource {
    Packages(NonEmpty<ResolvedPackage>),
    HostTree(HostTreeSnapshot),
}

/// A layer that has been identified but not necessarily built.
#[derive(Debug, Clone)]
pub struct PlannedLayer {
    key: LayerKey,
    kind: LayerKind,
    label: String,
    source: LayerSource,
}

impl PlannedLayer {
    fn packages(kind: LayerKind, members: NonEmpty<ResolvedPackage>, prefix: &EnvPrefix, layout: LayoutVersion, python: Option<PythonAbi>) -> Self {
        let keys: Vec<MemberKey> = members.iter().map(|p| MemberKey::of(p, python)).collect();
        let key = LayerKey::derive(layout, LayerKeyInput::Packages { prefix, members: &keys });
        let label = match kind {
            LayerKind::SmallGroup => format!("small packages ({})", members.len()),
            _ => members.first().display_name(),
        };
        Self { key, kind, label, source: LayerSource::Packages(members) }
    }

    pub fn host_tree(snapshot: HostTreeSnapshot, layout: LayoutVersion) -> Self {
        let key = LayerKey::derive(
            layout,
            LayerKeyInput::HostTree { mount_at: &snapshot.mount_at, tree: snapshot.digest },
        );
        let label = format!("{} → {}", snapshot.source, snapshot.mount_at);
        Self { key, kind: LayerKind::HostTree, label, source: LayerSource::HostTree(snapshot) }
    }

    pub fn key(&self) -> LayerKey {
        self.key
    }

    pub fn kind(&self) -> LayerKind {
        self.kind
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn source(&self) -> &LayerSource {
        &self.source
    }
}

/// Ordered layers (bottom → top) for one environment plus host trees.
///
/// Invariant: every package of the environment is in exactly one layer.
#[derive(Debug, Clone)]
pub struct LayerPlan {
    layers: Vec<PlannedLayer>,
}

#[derive(Debug, thiserror::Error)]
#[error("layer plan does not cover package `{0}` exactly once")]
pub struct PlanCoverageError(String);

impl LayerPlan {
    /// Checks that `layers` partitions `env`'s packages.
    pub fn new(env: &ResolvedEnvironment, layers: Vec<PlannedLayer>) -> Result<Self, PlanCoverageError> {
        let mut seen: BTreeMap<PackageSha256, usize> = BTreeMap::new();
        for layer in &layers {
            if let LayerSource::Packages(members) = &layer.source {
                for package in members {
                    *seen.entry(package.sha256()).or_default() += 1;
                }
            }
        }
        for package in env.packages() {
            if seen.remove(&package.sha256()) != Some(1) {
                return Err(PlanCoverageError(package.display_name()));
            }
        }
        if let Some((extra, _)) = seen.into_iter().next() {
            return Err(PlanCoverageError(extra.to_string()));
        }
        Ok(Self { layers })
    }

    /// Adds a host directory on top of the stack.
    pub fn push_host_tree(&mut self, snapshot: HostTreeSnapshot, layout: LayoutVersion) {
        self.layers.push(PlannedLayer::host_tree(snapshot, layout));
    }

    pub fn layers(&self) -> &[PlannedLayer] {
        &self.layers
    }
}

/// Packages below the threshold share one layer at the bottom of the stack;
/// every other package gets its own layer, in install order.
#[derive(Debug, Clone, Copy)]
pub struct SizeThresholdPolicy {
    pub threshold: ByteSize,
}

impl Default for SizeThresholdPolicy {
    fn default() -> Self {
        Self { threshold: ByteSize::mib(1) }
    }
}

impl SizeThresholdPolicy {
    pub fn plan(&self, env: &ResolvedEnvironment, prefix: &EnvPrefix, layout: LayoutVersion) -> LayerPlan {
        let python = env.python();
        let (small, big): (Vec<_>, Vec<_>) = env
            .packages()
            .iter()
            .cloned()
            .partition(|p| p.archive_size() < self.threshold);

        let mut layers = Vec::with_capacity(big.len() + 1);
        if let Some(group) = NonEmpty::new(small) {
            let kind = if group.len() == 1 { LayerKind::Package } else { LayerKind::SmallGroup };
            layers.push(PlannedLayer::packages(kind, group, prefix, layout, python));
        }
        for package in big {
            let members = NonEmpty::singleton(package);
            layers.push(PlannedLayer::packages(LayerKind::Package, members, prefix, layout, python));
        }
        LayerPlan::new(env, layers).expect("partition covers every package exactly once")
    }
}

/// A non-directory path a layer contains, and who put it there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub path: GuestPath,
    pub owner: FileOwner,
}

/// Non-directory paths of a built layer. Stored next to the layer so path
/// conflicts can be checked without rebuilding cached layers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerIndex {
    pub entries: Vec<IndexEntry>,
}

/// The same non-directory path provided by several owners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathConflict {
    pub path: GuestPath,
    pub owners: Vec<FileOwner>,
}

impl std::fmt::Display for PathConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let owners: Vec<String> = self.owners.iter().map(ToString::to_string).collect();
        write!(f, "{} is provided by {}", self.path, owners.join(", "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Refuse to build an image with conflicting paths.
    #[default]
    Reject,
    /// The layer higher in the stack wins (overlayfs semantics).
    UpperLayerWins,
}

impl ConflictPolicy {
    pub fn admits(self, conflicts: &PathConflicts) -> bool {
        self == Self::UpperLayerWins || conflicts.is_empty()
    }
}

/// Every path provided by more than one owner across a stack of layers,
/// sorted by path; owners are listed bottom → top.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathConflicts(Vec<PathConflict>);

impl PathConflicts {
    /// Conflicts listed in `Display` before the rest is summarized.
    const SHOWN: usize = 20;

    pub fn detect<'a>(indices: impl IntoIterator<Item = &'a LayerIndex>) -> Self {
        let mut owners: BTreeMap<&GuestPath, Vec<&FileOwner>> = BTreeMap::new();
        for index in indices {
            for entry in &index.entries {
                owners.entry(&entry.path).or_default().push(&entry.owner);
            }
        }
        let conflicts = owners
            .into_iter()
            .filter(|(_, owners)| owners.len() > 1)
            .map(|(path, owners)| PathConflict { path: path.clone(), owners: owners.into_iter().cloned().collect() })
            .collect();
        Self(conflicts)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn conflicts(&self) -> &[PathConflict] {
        &self.0
    }
}

impl std::fmt::Display for PathConflicts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} paths are provided by more than one layer:", self.0.len())?;
        for conflict in self.0.iter().take(Self::SHOWN) {
            write!(f, "\n  {conflict}")?;
        }
        if self.0.len() > Self::SHOWN {
            write!(f, "\n  … and {} more", self.0.len() - Self::SHOWN)?;
        }
        Ok(())
    }
}

/// A layer that exists in a layer store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltLayer {
    pub key: LayerKey,
    pub digest: LayerDigest,
    pub size: ByteSize,
    pub kind: LayerKind,
    pub label: String,
}

/// Freshly written layer bytes, not yet in a store.
#[derive(Debug)]
pub struct LayerBlob {
    /// Temporary file holding the SquashFS image; the store takes ownership.
    pub path: Utf8PathBuf,
    pub digest: LayerDigest,
    pub size: ByteSize,
    pub index: LayerIndex,
}

/// A built layer together with its path index.
#[derive(Debug, Clone)]
pub struct StoredLayer {
    pub layer: BuiltLayer,
    pub index: LayerIndex,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(byte: u8, python: Option<PythonAbi>) -> MemberKey {
        MemberKey { package: PackageSha256::from_bytes([byte; 32]), python }
    }

    #[test]
    fn package_key_ignores_member_order_but_not_inputs() {
        let prefix = EnvPrefix::default();
        let layout = LayoutVersion::CURRENT;
        let key = |members: &[MemberKey], prefix: &EnvPrefix, layout| {
            LayerKey::derive(layout, LayerKeyInput::Packages { prefix, members })
        };
        let a = member(1, None);
        let b = member(2, None);
        let base = key(&[a, b], &prefix, layout);

        assert_eq!(base, key(&[b, a], &prefix, layout));
        assert_ne!(base, key(&[a], &prefix, layout));
        assert_ne!(base, key(&[a, b], &prefix, LayoutVersion(2)));
        let other_prefix = EnvPrefix::new(GuestPath::new("/env").unwrap());
        assert_ne!(base, key(&[a, b], &other_prefix, layout));
        let py312 = PythonAbi { major: 3, minor: 12 };
        let py313 = PythonAbi { major: 3, minor: 13 };
        assert_ne!(key(&[member(1, Some(py312))], &prefix, layout), key(&[member(1, Some(py313))], &prefix, layout));
    }

    #[test]
    fn conflicts_report_every_owner_of_a_shared_path() {
        let path = GuestPath::new("/opt/env/bin/tool").unwrap();
        let index = |owner: &str, path: &GuestPath| LayerIndex {
            entries: vec![IndexEntry { path: path.clone(), owner: FileOwner::Package(owner.into()) }],
        };
        let unique = GuestPath::new("/opt/env/bin/other").unwrap();
        let conflicts = PathConflicts::detect([&index("a", &path), &index("b", &unique), &index("c", &path)]);
        assert_eq!(
            conflicts.conflicts(),
            &[PathConflict { path, owners: vec![FileOwner::Package("a".into()), FileOwner::Package("c".into())] }]
        );
        assert!(!ConflictPolicy::Reject.admits(&conflicts));
        assert!(ConflictPolicy::UpperLayerWins.admits(&conflicts));
        assert!(ConflictPolicy::Reject.admits(&PathConflicts::default()));
    }
}
