//! On-disk layout shared by every fervor host:
//!
//! ```text
//! <root>/blobs/sha256/<digest>   layer images, immutable, content-addressed
//! <root>/keys/<key>.json         build cache: layer key → { layer, index }
//! <root>/tmp/                    staging; same filesystem as blobs/ so renames are atomic
//! ```
//!
//! Other components keep their own subdirectories of the same root
//! (`artifacts/`, `packs/`, `pkgs/`).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::digest::{LayerDigest, LayerKey};
use fervor_domain::image::Image;
use fervor_domain::layer::{BuiltLayer, LayerBlob, LayerIndex, LayerKind, StoredLayer};
use fervor_domain::manifest::{MANIFEST_FILE, ManifestError, ManifestV1};
use rattler_digest::Sha256;
use serde::{Deserialize, Serialize};

/// Environment variable that overrides the store root.
pub const CACHE_DIR_ENV: &str = "FERVOR_CACHE_DIR";

/// Underlying failure carried in an error's `Infrastructure` variant.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("layer {digest} is corrupt: its bytes hash to {actual}")]
    Corrupt {
        digest: LayerDigest,
        actual: LayerDigest,
    },
    #[error("{context}")]
    Infrastructure {
        context: String,
        #[source]
        source: BoxError,
    },
}

impl StoreError {
    pub fn infrastructure(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Infrastructure {
            context: context.into(),
            source: source.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImageDirError {
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("{context}")]
    Infrastructure {
        context: String,
        #[source]
        source: BoxError,
    },
}

impl ImageDirError {
    pub fn infrastructure(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Infrastructure {
            context: context.into(),
            source: source.into(),
        }
    }
}

/// Where things live below a store root.
#[derive(Debug, Clone)]
struct StoreLayout {
    root: Utf8PathBuf,
}

impl StoreLayout {
    fn new(root: impl Into<Utf8PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn blobs_dir(&self) -> Utf8PathBuf {
        self.root.join("blobs/sha256")
    }

    fn keys_dir(&self) -> Utf8PathBuf {
        self.root.join("keys")
    }

    fn tmp_dir(&self) -> Utf8PathBuf {
        self.root.join("tmp")
    }

    fn blob_path(&self, digest: &LayerDigest) -> Utf8PathBuf {
        self.blobs_dir().join(digest.to_string())
    }

    fn key_path(&self, key: &LayerKey) -> Utf8PathBuf {
        self.keys_dir().join(format!("{key}.json"))
    }
}

/// Content-addressed layer storage plus the key → digest build cache.
#[derive(Debug, Clone)]
pub struct LayerStore {
    layout: StoreLayout,
}

impl LayerStore {
    pub fn open(root: impl Into<Utf8PathBuf>) -> Result<Self, StoreError> {
        let layout = StoreLayout::new(root);
        for dir in [layout.blobs_dir(), layout.keys_dir(), layout.tmp_dir()] {
            fs::create_dir_all(&dir)
                .map_err(|e| StoreError::infrastructure(format!("creating {dir}"), e))?;
        }
        Ok(Self { layout })
    }

    /// `$FERVOR_CACHE_DIR`, else `$XDG_CACHE_HOME/fervor` or `~/.cache/fervor`.
    pub fn default_root() -> Utf8PathBuf {
        if let Ok(dir) = std::env::var(CACHE_DIR_ENV) {
            return dir.into();
        }
        let base = std::env::var("XDG_CACHE_HOME")
            .map(Utf8PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").expect("HOME is set");
                Utf8PathBuf::from(home).join(".cache")
            });
        base.join("fervor")
    }

    pub fn root(&self) -> &Utf8Path {
        &self.layout.root
    }

    /// Staging directory for files that will be renamed into the store.
    pub fn tmp_dir(&self) -> Utf8PathBuf {
        self.layout.tmp_dir()
    }

    pub fn blob_path(&self, digest: &LayerDigest) -> Utf8PathBuf {
        self.layout.blob_path(digest)
    }

    /// Copies a blob from another store (e.g. the build host's store seen
    /// through a mount) unless it is already present. The copy is verified
    /// before it becomes visible.
    pub fn import_from(
        &self,
        source_root: &Utf8Path,
        digest: &LayerDigest,
    ) -> Result<(), StoreError> {
        if self.contains(digest)? {
            return Ok(());
        }
        let source = StoreLayout::new(source_root).blob_path(digest);
        let staged = StagedFile::create_in(
            &self.layout.tmp_dir(),
            format!("staging import of {digest}"),
        )?;
        fs::copy(&source, staged.path())
            .map_err(|e| StoreError::infrastructure(format!("copying {source}"), e))?;
        BlobFile::verify(staged.path(), digest)?;
        let target = self.blob_path(digest);
        staged.persist(&target, format!("storing {digest}"))?;
        BlobFile { path: target }.seal()
    }

    pub fn lookup(&self, key: &LayerKey) -> Result<Option<StoredLayer>, StoreError> {
        let Some(record) = KeyRecord::read(&self.layout.key_path(key))? else {
            return Ok(None);
        };
        // A key whose blob was removed is a cache miss, not an error.
        if !self.contains(&record.layer.digest)? {
            return Ok(None);
        }
        Ok(Some(record.into()))
    }

    /// Moves `blob` into the store and records it under `key`.
    pub fn insert(
        &self,
        key: LayerKey,
        kind: LayerKind,
        label: String,
        blob: LayerBlob,
    ) -> Result<StoredLayer, StoreError> {
        let target = self.blob_path(&blob.digest);
        if target.exists() {
            fs::remove_file(&blob.path)
                .map_err(|e| StoreError::infrastructure(format!("removing {}", blob.path), e))?;
        } else {
            BlobFile { path: blob.path }.move_into(target)?.seal()?;
        }

        let layer = BuiltLayer {
            key,
            digest: blob.digest,
            size: blob.size,
            kind,
            label,
        };
        let record = KeyRecord {
            layer,
            index: blob.index,
        };
        record.write(&self.layout.key_path(&key), &self.layout.tmp_dir())?;
        Ok(record.into())
    }

    pub fn contains(&self, digest: &LayerDigest) -> Result<bool, StoreError> {
        let path = self.blob_path(digest);
        path.try_exists()
            .map_err(|e| StoreError::infrastructure(format!("checking {path}"), e))
    }
}

/// Build cache entry: layer key → { layer, index }, at `keys/<key>.json`.
#[derive(Serialize, Deserialize)]
struct KeyRecord {
    layer: BuiltLayer,
    index: LayerIndex,
}

impl KeyRecord {
    /// `None` if no record exists at `path`.
    fn read(path: &Utf8Path) -> Result<Option<Self>, StoreError> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::infrastructure(format!("reading {path}"), e)),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| StoreError::infrastructure(format!("parsing {path}"), e))
    }

    /// Atomically replaces the record at `path`, staging it in `tmp_dir`.
    fn write(&self, path: &Utf8Path, tmp_dir: &Utf8Path) -> Result<(), StoreError> {
        let json = serde_json::to_vec(self)
            .map_err(|e| StoreError::infrastructure("serializing layer record", e))?;
        let staged = StagedFile::create_in(tmp_dir, format!("staging {path}"))?;
        fs::write(staged.path(), json)
            .map_err(|e| StoreError::infrastructure(format!("writing {path}"), e))?;
        staged.persist(path, format!("writing {path}"))
    }
}

impl From<KeyRecord> for StoredLayer {
    fn from(record: KeyRecord) -> Self {
        StoredLayer {
            layer: record.layer,
            index: record.index,
        }
    }
}

/// A content-addressed blob file.
#[derive(Debug)]
pub struct BlobFile {
    path: Utf8PathBuf,
}

impl BlobFile {
    /// sha256 of a file's bytes.
    pub fn digest_of(path: &Utf8Path) -> Result<LayerDigest, StoreError> {
        rattler_digest::compute_file_digest::<Sha256>(path)
            .map(LayerDigest::from_hash)
            .map_err(|e| StoreError::infrastructure(format!("hashing {path}"), e))
    }

    /// Fails with [`StoreError::Corrupt`] unless the file at `path` hashes to `expected`.
    fn verify(path: &Utf8Path, expected: &LayerDigest) -> Result<(), StoreError> {
        let actual = Self::digest_of(path)?;
        if actual != *expected {
            return Err(StoreError::Corrupt {
                digest: *expected,
                actual,
            });
        }
        Ok(())
    }

    /// Renames the blob to `to`, copying across filesystems.
    fn move_into(self, to: Utf8PathBuf) -> Result<Self, StoreError> {
        let from = self.path;
        match fs::rename(&from, &to) {
            Ok(()) => {}
            // Different filesystem: copy, then drop the original.
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                fs::copy(&from, &to)
                    .map_err(|e| StoreError::infrastructure(format!("copying {from}"), e))?;
                fs::remove_file(&from)
                    .map_err(|e| StoreError::infrastructure(format!("removing {from}"), e))?;
            }
            Err(e) => {
                return Err(StoreError::infrastructure(
                    format!("moving {from} to {to}"),
                    e,
                ));
            }
        }
        Ok(Self { path: to })
    }

    /// Blobs are immutable: drop write permission, keep them world-readable.
    fn seal(&self) -> Result<(), StoreError> {
        let path = &self.path;
        fs::set_permissions(path, fs::Permissions::from_mode(0o444))
            .map_err(|e| StoreError::infrastructure(format!("chmod {path}"), e))
    }
}

/// A temporary file in the store's tmp dir that becomes visible at its
/// final path only through an atomic rename; dropped unpersisted, it is removed.
struct StagedFile {
    file: tempfile::NamedTempFile,
}

impl StagedFile {
    fn create_in(tmp_dir: &Utf8Path, context: String) -> Result<Self, StoreError> {
        let file = tempfile::NamedTempFile::new_in(tmp_dir)
            .map_err(|e| StoreError::infrastructure(context, e))?;
        Ok(Self { file })
    }

    fn path(&self) -> &Utf8Path {
        Utf8Path::from_path(self.file.path()).expect("utf-8 tmp dir")
    }

    fn persist(self, to: &Utf8Path, context: String) -> Result<(), StoreError> {
        self.file
            .persist(to)
            .map_err(|e| StoreError::infrastructure(context, e.error))?;
        Ok(())
    }
}

/// An image directory holding the image as `<path>/manifest.json`.
#[derive(Debug, Clone)]
pub struct ImageDir {
    path: Utf8PathBuf,
}

impl ImageDir {
    pub fn new(path: impl Into<Utf8PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Utf8Path {
        &self.path
    }

    pub fn save(&self, image: &Image) -> Result<(), ImageDirError> {
        let dir = &self.path;
        fs::create_dir_all(dir)
            .map_err(|e| ImageDirError::infrastructure(format!("creating {dir}"), e))?;
        let mut json = serde_json::to_vec_pretty(&ManifestV1::from(image))
            .map_err(|e| ImageDirError::infrastructure("serializing manifest", e))?;
        json.push(b'\n');
        let path = dir.join(MANIFEST_FILE);
        fs::write(&path, json)
            .map_err(|e| ImageDirError::infrastructure(format!("writing {path}"), e))
    }

    pub fn load(&self) -> Result<Image, ImageDirError> {
        let path = self.path.join(MANIFEST_FILE);
        let bytes = fs::read(&path)
            .map_err(|e| ImageDirError::infrastructure(format!("reading {path}"), e))?;
        let manifest: ManifestV1 = serde_json::from_slice(&bytes)
            .map_err(|e| ImageDirError::infrastructure(format!("parsing {path}"), e))?;
        Ok(Image::try_from(manifest)?)
    }
}

#[cfg(test)]
mod tests {
    use fervor_domain::size::ByteSize;

    use super::*;

    fn blob(store: &LayerStore, bytes: &[u8]) -> LayerBlob {
        let path = store.tmp_dir().join("blob");
        fs::write(&path, bytes).unwrap();
        LayerBlob {
            digest: BlobFile::digest_of(&path).unwrap(),
            path,
            size: ByteSize::bytes(bytes.len() as u64),
            index: LayerIndex::default(),
        }
    }

    fn temp_store() -> (tempfile::TempDir, LayerStore) {
        let dir = tempfile::tempdir().unwrap();
        let store =
            LayerStore::open(Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap()).unwrap();
        (dir, store)
    }

    #[test]
    fn inserted_layers_are_found_by_key_until_their_blob_disappears() {
        let (_dir, store) = temp_store();
        let key = LayerKey::from_bytes([7; 32]);
        assert!(store.lookup(&key).unwrap().is_none());

        let blob = blob(&store, b"layer bytes");
        let digest = blob.digest;
        store
            .insert(key, LayerKind::Package, "pkg".into(), blob)
            .unwrap();
        assert_eq!(store.lookup(&key).unwrap().unwrap().layer.digest, digest);

        fs::remove_file(store.blob_path(&digest)).unwrap();
        assert!(store.lookup(&key).unwrap().is_none());
    }

    #[test]
    fn import_verifies_the_copied_bytes() {
        let (_src_dir, source) = temp_store();
        let (_dst_dir, target) = temp_store();
        let blob = blob(&source, b"good bytes");
        let digest = blob.digest;
        source
            .insert(
                LayerKey::from_bytes([1; 32]),
                LayerKind::Package,
                "pkg".into(),
                blob,
            )
            .unwrap();

        target.import_from(source.root(), &digest).unwrap();
        assert!(target.contains(&digest).unwrap());

        let tampered = LayerDigest::from_bytes([9; 32]);
        fs::copy(source.blob_path(&digest), source.blob_path(&tampered)).unwrap();
        assert!(matches!(
            target.import_from(source.root(), &tampered),
            Err(StoreError::Corrupt { .. })
        ));
        assert!(!target.contains(&tampered).unwrap());
    }
}
