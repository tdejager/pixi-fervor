//! The files that make up a layer, independent of where they came from
//! (package archive, host directory, sysroot).

use camino::Utf8PathBuf;
use rattler_conda_types::PackageName;

use crate::path::GuestPath;

/// Permission bits (`0o7777` at most). Ownership is always `root:root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileMode(u16);

impl FileMode {
    pub const DIRECTORY: Self = Self(0o755);
    pub const EXECUTABLE: Self = Self(0o755);
    pub const REGULAR: Self = Self(0o644);

    pub fn new(bits: u32) -> Self {
        Self((bits & 0o7777) as u16)
    }

    pub fn bits(self) -> u16 {
        self.0
    }
}

/// Bytes of a regular file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Bytes(Vec<u8>),
    /// Read lazily from the host when the layer is written.
    HostFile(Utf8PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    File { content: FileContent, mode: FileMode },
    Symlink { target: String },
    Directory { mode: FileMode },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerEntry {
    pub path: GuestPath,
    pub kind: EntryKind,
}

/// Who contributed an entry; reported in path conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum FileOwner {
    Package(String),
    HostTree(GuestPath),
    Boot,
}

impl FileOwner {
    pub fn package(name: &PackageName) -> Self {
        Self::Package(name.as_normalized().to_owned())
    }
}

impl std::fmt::Display for FileOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Package(name) => write!(f, "package {name}"),
            Self::HostTree(path) => write!(f, "host tree at {path}"),
            Self::Boot => write!(f, "boot layer"),
        }
    }
}

/// All entries one owner contributes to a layer.
#[derive(Debug, Clone)]
pub struct FileSet {
    pub owner: FileOwner,
    pub entries: Vec<LayerEntry>,
}

/// An entry exactly as stored in a package archive (path relative to the
/// archive root), before any install-time relocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub path: String,
    pub kind: EntryKind,
}
