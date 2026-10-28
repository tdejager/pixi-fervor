//! Run-time files (`run --copy`, `run --file`): built on the client into
//! content-addressed host-tree layers, exactly like build-time copies, so a
//! repeated run reuses them.

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::boot::hidden_by_runtime_mount;
use fervor_domain::layer::{BuiltLayer, LayerSource, LayoutVersion, PlannedLayer};
use fervor_domain::path::GuestPath;
use fervor_layerfs::{BuildError, HostTreeError, HostTreeReader, SquashfsLayerBuilder};
use fervor_store::{LayerStore, StoreError};

use crate::HostTreeMount;

/// Host files added to the guest for one run.
#[derive(Debug, Clone)]
pub enum RunFile {
    /// A host directory copied to a guest directory.
    Tree(HostTreeMount),
    /// A single host file copied to the guest path `path`.
    File { source: Utf8PathBuf, path: GuestPath },
}

#[derive(Debug, thiserror::Error)]
pub enum RunLayerError {
    #[error(transparent)]
    HostTree(#[from] HostTreeError),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{path} is below {mount}, which the guest mounts afresh at boot, so the file would be hidden; pick another destination")]
    HiddenDestination { path: GuestPath, mount: &'static str },
}

/// Builds run-time files into layers of a layer store.
pub struct RunLayerBuilder {
    layers: SquashfsLayerBuilder,
    store: LayerStore,
    layout: LayoutVersion,
}

impl RunLayerBuilder {
    /// Layers are stored in the store at `cache_root`.
    pub fn new(cache_root: &Utf8Path) -> Result<Self, StoreError> {
        let store = LayerStore::open(cache_root)?;
        Ok(Self { layers: SquashfsLayerBuilder::new(store.tmp_dir()), store, layout: LayoutVersion::CURRENT })
    }

    /// One layer per file, in the given order (bottom → top), reusing layers
    /// already in the store. Blocking: hashes and writes the files.
    pub fn build(&self, files: &[RunFile]) -> Result<Vec<BuiltLayer>, RunLayerError> {
        files.iter().map(|file| self.layer(file)).collect()
    }

    fn layer(&self, file: &RunFile) -> Result<BuiltLayer, RunLayerError> {
        let destination = match file {
            RunFile::Tree(mount) => &mount.mount_at,
            RunFile::File { path, .. } => path,
        };
        if let Some(mount) = hidden_by_runtime_mount(destination) {
            return Err(RunLayerError::HiddenDestination { path: destination.clone(), mount });
        }
        let snapshot = match file {
            RunFile::Tree(mount) => HostTreeReader::snapshot(&mount.source, &mount.mount_at)?,
            RunFile::File { source, path } => HostTreeReader::snapshot_file(source, path)?,
        };
        let planned = PlannedLayer::host_tree(snapshot, self.layout);
        if let Some(stored) = self.store.lookup(&planned.key())? {
            return Ok(stored.layer);
        }
        let LayerSource::HostTree(snapshot) = planned.source() else {
            unreachable!("a host-tree layer's source is its snapshot")
        };
        let blob = self.layers.build(std::slice::from_ref(&snapshot.files))?;
        let stored = self.store.insert(planned.key(), planned.kind(), planned.label().to_owned(), blob)?;
        Ok(stored.layer)
    }
}
