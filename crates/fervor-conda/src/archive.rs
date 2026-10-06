//! Reads a conda package archive into memory. Nothing is ever extracted to
//! the host filesystem: package paths may collide on case-insensitive hosts.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use camino::Utf8Path;
use fervor_domain::files::{EntryKind, FileContent, FileMode};
use rattler_conda_types::package::CondaArchiveType;
use rattler_package_streaming::ExtractError;
use rattler_package_streaming::read::stream_tar_bz2;
use rattler_package_streaming::seek::{stream_conda_content, stream_conda_info};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("`{0}` is neither a `.conda` nor a `.tar.bz2` archive")]
    UnsupportedFormat(String),
    #[error("failed to read the package archive")]
    Read(#[from] ExtractError),
    #[error("failed to read the package archive")]
    Io(#[from] std::io::Error),
    #[error("archive entry `{0}` has a path that leaves the package")]
    UnsafePath(String),
    #[error("archive entry `{0}` is not a file, directory or link")]
    UnsupportedEntry(String),
    #[error("hard link `{path}` points to `{target}`, which is not a regular file in the archive")]
    DanglingHardlink { path: String, target: String },
}

/// The decoded contents of one package archive.
#[derive(Debug, Default)]
pub struct PackageArchive {
    /// Raw bytes of the regular files under `info/`, keyed by archive path.
    pub info: BTreeMap<String, Vec<u8>>,
    /// Every non-`info/` entry, keyed by archive path. Hard links are
    /// resolved to copies of the file they point to.
    pub content: BTreeMap<String, EntryKind>,
}

enum RawEntry {
    Resolved(EntryKind),
    Hardlink { target: String, mode: FileMode },
}

impl PackageArchive {
    /// Reads `.conda` or `.tar.bz2` (decided by the file name).
    pub fn read(path: &Utf8Path) -> Result<Self, ArchiveError> {
        let archive_type =
            CondaArchiveType::try_from(path).ok_or_else(|| ArchiveError::UnsupportedFormat(path.to_string()))?;
        let mut raw = BTreeMap::new();
        let mut archive = Self::default();
        match archive_type {
            CondaArchiveType::TarBz2 => {
                archive.collect(stream_tar_bz2(File::open(path)?), &mut raw)?;
            }
            CondaArchiveType::Conda => {
                archive.collect(stream_conda_info(File::open(path)?)?, &mut raw)?;
                archive.collect(stream_conda_content(File::open(path)?)?, &mut raw)?;
            }
        }
        archive.resolve_hardlinks(raw)?;
        Ok(archive)
    }

    fn collect(&mut self, mut tar: tar::Archive<impl Read>, raw: &mut BTreeMap<String, RawEntry>) -> Result<(), ArchiveError> {
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = ArchivePath::new(&entry.path()?)?;
            if path.is_package_root() {
                continue;
            }
            let is_info = path.is_info();
            let path = path.into_string();
            let header = entry.header();
            let mode = FileMode::new(header.mode()?);
            let entry_type = header.entry_type();
            let link_target = || -> Result<String, ArchiveError> {
                let target = entry.link_name()?.ok_or_else(|| ArchiveError::UnsupportedEntry(path.clone()))?;
                target.to_str().map(str::to_owned).ok_or_else(|| ArchiveError::UnsupportedEntry(path.clone()))
            };
            let kind = if entry_type.is_symlink() {
                RawEntry::Resolved(EntryKind::Symlink { target: link_target()? })
            } else if entry_type.is_hard_link() {
                RawEntry::Hardlink { target: ArchivePath::new(Path::new(&link_target()?))?.into_string(), mode }
            } else if entry_type.is_dir() {
                RawEntry::Resolved(EntryKind::Directory { mode })
            } else if entry_type.is_file() {
                let mut bytes = Vec::with_capacity(entry.size().min(64 << 20) as usize);
                entry.read_to_end(&mut bytes)?;
                if is_info {
                    self.info.insert(path, bytes);
                    continue;
                }
                RawEntry::Resolved(EntryKind::File { content: FileContent::Bytes(bytes), mode })
            } else {
                return Err(ArchiveError::UnsupportedEntry(path));
            };
            if !is_info {
                raw.insert(path, kind);
            }
        }
        Ok(())
    }

    fn resolve_hardlinks(&mut self, raw: BTreeMap<String, RawEntry>) -> Result<(), ArchiveError> {
        let mut hardlinks = Vec::new();
        for (path, entry) in raw {
            match entry {
                RawEntry::Resolved(kind) => {
                    self.content.insert(path, kind);
                }
                RawEntry::Hardlink { target, mode } => hardlinks.push((path, target, mode)),
            }
        }
        for (path, target, mode) in hardlinks {
            let Some(EntryKind::File { content, .. }) = self.content.get(&target) else {
                return Err(ArchiveError::DanglingHardlink { path, target });
            };
            let kind = EntryKind::File { content: content.clone(), mode };
            self.content.insert(path, kind);
        }
        Ok(())
    }
}

/// A path inside a package archive as `a/b/c`: `.` components dropped,
/// anything escaping the package rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePath(String);

impl ArchivePath {
    pub fn new(path: &Path) -> Result<Self, ArchiveError> {
        let unsafe_path = || ArchiveError::UnsafePath(path.to_string_lossy().into_owned());
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => parts.push(part.to_str().ok_or_else(unsafe_path)?),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => return Err(unsafe_path()),
            }
        }
        Ok(Self(parts.join("/")))
    }

    /// The package root itself (`.`), which carries no entry.
    pub fn is_package_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Package metadata under `info/` rather than installed content.
    pub fn is_info(&self) -> bool {
        self.0 == "info" || self.0.starts_with("info/")
    }

    pub fn into_string(self) -> String {
        self.0
    }
}
