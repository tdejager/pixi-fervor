//! What a package installs into a prefix, computed from its archive in
//! memory: the conda link step without touching the host filesystem.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use fervor_domain::environment::PythonAbi;
use fervor_domain::files::{EntryKind, FileContent, FileMode, FileOwner, FileSet, LayerEntry};
use fervor_domain::layer::EnvPrefix;
use fervor_domain::path::{GuestPath, GuestPathError};
use fervor_domain::platform::GuestPlatform;
use rattler::install::PythonInfo;
use rattler::install::link::copy_and_replace_placeholders;
use rattler::install::python_entry_point_template;
use rattler_conda_types::package::{
    EntryPoint, Files, HasPrefix, LinkJson, NoArchLinks, NoLink, NoSoftlink, PackageFile, PathType, PathsEntry,
    PathsJson,
};
use rattler_conda_types::prefix_record::{self, PrefixRecord};
use rattler_conda_types::{RepoDataRecord, Version};
use rattler_digest::{Sha256, compute_bytes_digest};

use crate::archive::PackageArchive;

/// Where and for whom package files are installed.
#[derive(Debug, Clone)]
pub struct InstallTarget {
    pub prefix: EnvPrefix,
    pub platform: GuestPlatform,
    /// Required for `noarch: python` packages.
    pub python: Option<PythonAbi>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("the package is noarch: python but the environment has no python")]
    MissingPython,
    #[error("failed to parse `{path}`")]
    Metadata {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("the package has neither `info/paths.json` nor `info/files`")]
    MissingFileList,
    #[error("`{0}` is listed in the package metadata but missing from the archive")]
    MissingEntry(String),
    #[error("`{path}` is listed as a {expected} but the archive stores something else")]
    EntryMismatch { path: String, expected: &'static str },
    #[error("package path `{0}` is not valid UTF-8")]
    NonUtf8Path(String),
    #[error("package path cannot be placed in the prefix")]
    GuestPath(#[from] GuestPathError),
    #[error("failed to replace the prefix placeholder in `{path}`")]
    Placeholder {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to compute python install locations")]
    Python(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("failed to serialize the conda-meta record")]
    PrefixRecord(#[source] std::io::Error),
}

/// One package being linked into a prefix: its archive, its repodata
/// record and the environment it is installed into.
pub struct PackageInstall<'a> {
    record: &'a RepoDataRecord,
    archive: PackageArchive,
    target: &'a InstallTarget,
}

impl<'a> PackageInstall<'a> {
    pub fn new(record: &'a RepoDataRecord, archive: PackageArchive, target: &'a InstallTarget) -> Self {
        Self { record, archive, target }
    }

    /// Entries the package installs into `target.prefix`, ending with its
    /// `conda-meta/<name>-<version>-<build>.json` record.
    pub fn files(mut self) -> Result<FileSet, InstallError> {
        let noarch = NoarchPython::for_package(self.record, self.target)?;
        let paths = self.paths_json()?;
        let mut entries = Vec::with_capacity(paths.paths.len() + 1);
        let mut installed = Vec::with_capacity(paths.paths.len() + 1);

        for entry in paths.paths {
            let (layer_entry, paths_entry) = self.link(entry, noarch.as_ref())?;
            entries.push(layer_entry);
            installed.push(paths_entry);
        }

        if let Some(noarch) = &noarch {
            let prefix = self.target.prefix.path();
            for entry_point in self.entry_points()? {
                let script = noarch.entry_point_script(&entry_point, prefix);
                installed.push(script.paths_entry());
                entries.push(script.into_layer_entry(prefix)?);
            }
        }

        entries.push(self.conda_meta(installed)?);
        Ok(FileSet { owner: FileOwner::package(&self.record.package_record.name), entries })
    }

    /// Links one `paths.json` entry: takes its archive entry, relocates it
    /// for noarch python and replaces the prefix placeholder. Returns the
    /// layer entry and its conda-meta record.
    fn link(
        &mut self,
        entry: PathsEntry,
        noarch: Option<&NoarchPython>,
    ) -> Result<(LayerEntry, prefix_record::PathsEntry), InstallError> {
        let prefix = self.target.prefix.path();
        let subdir = self.target.platform.subdir();
        let source = Self::utf8(&entry.relative_path)?.to_owned();
        let destination = match noarch {
            Some(noarch) => noarch.relocate(&entry.relative_path),
            None => entry.relative_path.clone(),
        };
        let destination_str = Self::utf8(&destination)?;
        let stored = self.archive.content.remove(&source);

        let mut sha256_in_prefix = None;
        let mut size_in_bytes = entry.size_in_bytes;
        let (kind, path_type) = match (entry.path_type, stored) {
            (PathType::Directory, Some(EntryKind::Directory { mode })) => {
                (EntryKind::Directory { mode }, prefix_record::PathType::Directory)
            }
            (PathType::Directory, None) => {
                (EntryKind::Directory { mode: FileMode::DIRECTORY }, prefix_record::PathType::Directory)
            }
            (PathType::Directory, Some(_)) => {
                return Err(InstallError::EntryMismatch { path: source, expected: "directory" });
            }
            (_, Some(EntryKind::Symlink { target })) => {
                (EntryKind::Symlink { target }, prefix_record::PathType::SoftLink)
            }
            (_, Some(EntryKind::File { content: FileContent::Bytes(bytes), mode })) => {
                let bytes = match &entry.prefix_placeholder {
                    Some(placeholder) => {
                        let mut replaced = Vec::with_capacity(bytes.len());
                        copy_and_replace_placeholders(
                            &bytes,
                            &mut replaced,
                            &placeholder.placeholder,
                            prefix.as_str(),
                            &subdir,
                            placeholder.file_mode,
                        )
                        .map_err(|source_error| InstallError::Placeholder {
                            path: source.clone(),
                            source: source_error,
                        })?;
                        sha256_in_prefix = Some(compute_bytes_digest::<Sha256>(&replaced));
                        size_in_bytes = Some(replaced.len() as u64);
                        replaced
                    }
                    None => bytes,
                };
                (
                    EntryKind::File { content: FileContent::Bytes(bytes), mode },
                    entry.path_type.into(),
                )
            }
            (_, Some(_)) => return Err(InstallError::EntryMismatch { path: source, expected: "file" }),
            (_, None) => return Err(InstallError::MissingEntry(source)),
        };

        let layer_entry = LayerEntry { path: prefix.join(destination_str)?, kind };
        let paths_entry = prefix_record::PathsEntry {
            original_path: (destination != entry.relative_path).then(|| entry.relative_path.clone()),
            relative_path: destination,
            path_type,
            no_link: entry.no_link,
            sha256: entry.sha256,
            sha256_in_prefix,
            size_in_bytes,
            file_mode: entry.prefix_placeholder.as_ref().map(|p| p.file_mode),
            prefix_placeholder: entry.prefix_placeholder.map(|p| p.placeholder),
        };
        Ok((layer_entry, paths_entry))
    }

    fn conda_meta(&self, installed: Vec<prefix_record::PathsEntry>) -> Result<LayerEntry, InstallError> {
        let prefix_record = PrefixRecord::from_repodata_record(self.record.clone(), installed);
        let mut json = Vec::new();
        prefix_record.write_to(&mut json, true).map_err(InstallError::PrefixRecord)?;
        Ok(LayerEntry {
            path: self.target.prefix.path().join(&format!("conda-meta/{}", prefix_record.file_name()))?,
            kind: EntryKind::File { content: FileContent::Bytes(json), mode: FileMode::REGULAR },
        })
    }

    /// `info/paths.json`, or the equivalent rebuilt from the pre-`paths.json`
    /// metadata files of old packages.
    fn paths_json(&self) -> Result<PathsJson, InstallError> {
        if let Some(paths) = self.metadata::<PathsJson>("info/paths.json")? {
            return Ok(paths);
        }
        let files = self.metadata::<Files>("info/files")?.ok_or(InstallError::MissingFileList)?;
        let has_prefix = self.metadata::<HasPrefix>("info/has_prefix")?;
        let no_link = self.metadata::<NoLink>("info/no_link")?;
        let no_softlink = self.metadata::<NoSoftlink>("info/no_softlink")?;
        PathsJson::from_deprecated(files, has_prefix, no_link, no_softlink, |path: &Path| {
            let path = Self::utf8(path)?;
            match self.archive.content.get(path) {
                Some(EntryKind::Symlink { .. }) => Ok(PathType::SoftLink),
                Some(EntryKind::Directory { .. }) => Ok(PathType::Directory),
                Some(EntryKind::File { .. }) => Ok(PathType::HardLink),
                None => Err(InstallError::MissingEntry(path.to_owned())),
            }
        })
    }

    fn entry_points(&self) -> Result<Vec<EntryPoint>, InstallError> {
        Ok(match self.metadata::<LinkJson>("info/link.json")? {
            Some(LinkJson { noarch: NoArchLinks::Python(links), .. }) => links.entry_points,
            _ => Vec::new(),
        })
    }

    /// The parsed `info/` metadata file at `path`, if the package has one.
    fn metadata<P: PackageFile>(&self, path: &'static str) -> Result<Option<P>, InstallError> {
        debug_assert_eq!(P::package_path(), Path::new(path));
        self.archive
            .info
            .get(path)
            .map(|bytes| P::from_slice(bytes).map_err(|source| InstallError::Metadata { path, source }))
            .transpose()
    }

    fn utf8(path: &Path) -> Result<&str, InstallError> {
        path.to_str().ok_or_else(|| InstallError::NonUtf8Path(path.to_string_lossy().into_owned()))
    }
}

/// Install locations of a `noarch: python` package in an environment with
/// python: where its `site-packages/` and `python-scripts/` land, and
/// where its entry points go.
struct NoarchPython(PythonInfo);

impl NoarchPython {
    /// `None` for packages that are not `noarch: python`.
    fn for_package(record: &RepoDataRecord, target: &InstallTarget) -> Result<Option<Self>, InstallError> {
        if !record.package_record.noarch.is_python() {
            return Ok(None);
        }
        let abi = target.python.ok_or(InstallError::MissingPython)?;
        let version = Version::from_str(&format!("{}.{}", abi.major, abi.minor))
            .map_err(|e| InstallError::Python(e.into()))?;
        let info = PythonInfo::from_version(&version, Some(&abi.site_packages()), target.platform.subdir())
            .map_err(|e| InstallError::Python(e.into()))?;
        Ok(Some(Self(info)))
    }

    /// Where the package path `path` is installed in the prefix.
    fn relocate(&self, path: &Path) -> PathBuf {
        self.0.get_python_noarch_target_path(path).into_owned()
    }

    fn entry_point_script(&self, entry_point: &EntryPoint, prefix: &GuestPath) -> EntryPointScript {
        EntryPointScript {
            relative_path: self.0.bin_dir.join(&entry_point.command),
            script: python_entry_point_template(prefix.as_str(), false, entry_point, &self.0).into_bytes(),
        }
    }
}

/// The generated launcher script of a python entry point, at its
/// prefix-relative path.
struct EntryPointScript {
    relative_path: PathBuf,
    script: Vec<u8>,
}

impl EntryPointScript {
    const MODE: FileMode = FileMode::EXECUTABLE;

    fn paths_entry(&self) -> prefix_record::PathsEntry {
        prefix_record::PathsEntry {
            relative_path: self.relative_path.clone(),
            original_path: None,
            path_type: prefix_record::PathType::UnixPythonEntryPoint,
            no_link: false,
            sha256: Some(compute_bytes_digest::<Sha256>(&self.script)),
            sha256_in_prefix: None,
            size_in_bytes: Some(self.script.len() as u64),
            file_mode: None,
            prefix_placeholder: None,
        }
    }

    fn into_layer_entry(self, prefix: &GuestPath) -> Result<LayerEntry, InstallError> {
        Ok(LayerEntry {
            path: prefix.join(PackageInstall::utf8(&self.relative_path)?)?,
            kind: EntryKind::File { content: FileContent::Bytes(self.script), mode: Self::MODE },
        })
    }
}
