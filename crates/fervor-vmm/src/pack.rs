//! The pack devices: a `PackHeader` followed by layers at aligned offsets.
//! The image pack (every non-boot layer) is cached at
//! `<root>/packs/<image-id>.img`; the run pack (run-time layers and per-run
//! config) is written into each run's directory.

use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::digest::LayerDigest;
use fervor_domain::image::Image;
use fervor_domain::machine::RunLayer;
use fervor_guest_abi::{GuestConfig, JournalConfig, PackConfig, PackError, PackHeader, RunConfig, ServiceConfig};
use fervor_store::LayerStore;
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, Default)]
pub struct Packer;

#[derive(Debug, thiserror::Error)]
pub enum PackerError {
    #[error(transparent)]
    Header(#[from] PackError),
    #[error("layer {digest} is {actual} bytes in the store, but {expected} bytes are recorded for it")]
    SizeMismatch { digest: LayerDigest, expected: u64, actual: u64 },
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl PackerError {
    fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io { context: context.into(), source }
    }
}

impl Packer {
    pub fn pack_path(&self, image: &Image, store: &LayerStore) -> Utf8PathBuf {
        store.root().join("packs").join(format!("{}.img", image.id()))
    }

    /// Writes the pack for `image` unless it exists. Every layer blob must be
    /// in `store`. The image id covers all layer digests, so an existing pack
    /// is always the right one.
    pub fn pack(&self, image: &Image, store: &LayerStore) -> Result<Utf8PathBuf, PackerError> {
        let target = self.pack_path(image, store);
        if target.try_exists().map_err(|e| PackerError::io(format!("checking {target}"), e))? {
            return Ok(target);
        }
        let packs = store.root().join("packs");
        fs::create_dir_all(&packs).map_err(|e| PackerError::io(format!("creating {packs}"), e))?;

        let mut staged = NamedTempFile::new_in(store.tmp_dir()).map_err(|e| PackerError::io("staging pack", e))?;
        let out = staged.as_file_mut();
        let layers = image.layers().layers();
        let header = PackHeader::layout(layers.iter().map(|l| (l.digest.to_bytes(), l.size.as_u64())), guest_config(image))?;
        write_pack(out, &header, store, &target)?;
        out.sync_all().map_err(|e| PackerError::io(format!("writing {target}"), e))?;
        staged.persist(&target).map_err(|e| PackerError::io(format!("storing {target}"), e.error))?;
        Ok(target)
    }

    /// Writes the run pack of one boot to the new file `target`: `layers`
    /// (bottom → top, every blob in `store`) behind a header carrying
    /// `config`. Header-only when there are no layers.
    pub fn pack_run(&self, layers: &[RunLayer], config: RunConfig, store: &LayerStore, target: &Utf8Path) -> Result<(), PackerError> {
        let header = PackHeader::layout(layers.iter().map(|l| (l.digest.to_bytes(), l.size.as_u64())), config)?;
        let mut out = File::create_new(target).map_err(|e| PackerError::io(format!("creating {target}"), e))?;
        write_pack(&mut out, &header, store, target)
    }
}

/// Writes `header`, then every layer blob it lists at its aligned offset,
/// into the empty file `out` (written to `target`).
fn write_pack<C: PackConfig>(out: &mut File, header: &PackHeader<C>, store: &LayerStore, target: &Utf8Path) -> Result<(), PackerError> {
    let write_err = |e| PackerError::io(format!("writing {target}"), e);
    out.write_all(&header.encode()?).map_err(write_err)?;
    for entry in header.layers() {
        let digest = LayerDigest::from_bytes(entry.digest);
        let blob = store.blob_path(&digest);
        let mut src = File::open(&blob).map_err(|e| PackerError::io(format!("opening {blob}"), e))?;
        let actual = src.metadata().map_err(|e| PackerError::io(format!("reading {blob}"), e))?.len();
        if actual != entry.len {
            return Err(PackerError::SizeMismatch { digest, expected: entry.len, actual });
        }
        // Grow to the aligned offset first so the copy appends at EOF. On
        // Linux, `io::copy` between files uses copy_file_range, which
        // reflinks on filesystems that support it and falls back to an
        // in-kernel or userspace copy otherwise.
        out.set_len(entry.offset).map_err(write_err)?;
        out.seek(SeekFrom::Start(entry.offset)).map_err(write_err)?;
        let copied = io::copy(&mut src, out).map_err(|e| PackerError::io(format!("copying {blob} into {target}"), e))?;
        if copied != entry.len {
            return Err(PackerError::SizeMismatch { digest, expected: entry.len, actual: copied });
        }
    }
    // Firecracker exposes whole sectors only; pad the tail to alignment.
    out.set_len(header.pack_len()).map_err(write_err)
}

/// What `fervor-init` receives in the pack header.
fn guest_config(image: &Image) -> GuestConfig {
    GuestConfig {
        argv: image.entrypoint().argv().args().to_vec(),
        env: image
            .entrypoint()
            .env()
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        workdir: image.entrypoint().workdir().to_string(),
        scratch_size_mib: image.scratch().mib(),
        services: image
            .services()
            .iter()
            .map(|s| ServiceConfig { name: s.name.to_string(), argv: s.argv.args().to_vec() })
            .collect(),
        journal: image.journal().map(|j| JournalConfig { daemon: j.daemon.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use fervor_domain::boot::InitVersion;
    use fervor_domain::digest::LayerKey;
    use fervor_domain::entrypoint::{Argv, EnvVars, Entrypoint};
    use fervor_domain::image::{LayerStack, ScratchSize};
    use fervor_domain::layer::{BuiltLayer, EnvPrefix, LayerKind};
    use fervor_domain::nonempty::NonEmpty;
    use fervor_domain::path::GuestPath;
    use fervor_domain::platform::GuestPlatform;
    use fervor_domain::service::{Journal, Service, Services};
    use fervor_domain::size::ByteSize;
    use fervor_guest_abi::pack::PACK_ALIGN;

    use super::*;

    fn layer_from(store: &LayerStore, kind: LayerKind, bytes: &[u8]) -> BuiltLayer {
        let digest = LayerDigest::of_bytes(bytes);
        fs::write(store.blob_path(&digest), bytes).unwrap();
        BuiltLayer {
            key: LayerKey::from_bytes(digest.to_bytes()),
            digest,
            size: ByteSize::bytes(bytes.len() as u64),
            kind,
            label: format!("{kind:?} {}", bytes.len()),
        }
    }

    fn image(store: &LayerStore, layers: &[&[u8]]) -> Image {
        image_with(store, layers, Services::default(), None)
    }

    fn image_with(store: &LayerStore, layers: &[&[u8]], services: Services, journal: Option<Journal>) -> Image {
        let argv = Argv::new(NonEmpty::new(vec!["python".into(), "app.py".into()]).unwrap());
        let entrypoint = Entrypoint::new(argv, EnvVars::default(), GuestPath::new("/app").unwrap(), &EnvPrefix::default());
        Image::new(
            GuestPlatform::LinuxAarch64,
            layer_from(store, LayerKind::Boot, b"boot layer"),
            InitVersion::CURRENT,
            LayerStack::new(layers.iter().map(|b| layer_from(store, LayerKind::Package, b)).collect()).unwrap(),
            entrypoint,
            services,
            journal,
            ScratchSize::default(),
            crate::artifacts::pinned_artifacts(GuestPlatform::LinuxAarch64),
        )
        .unwrap()
    }

    #[test]
    fn guest_config_carries_services_in_order_and_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap()).unwrap();
        let service = |name: &str, argv: &[&str]| Service {
            name: name.parse().unwrap(),
            argv: Argv::new(NonEmpty::new(argv.iter().map(|a| a.to_string()).collect()).unwrap()),
        };
        let services = Services::new(vec![service("web", &["python", "-m", "http.server"]), service("tick", &["sleep", "1"])]).unwrap();
        let journal = Journal { daemon: GuestPath::new("/opt/env/lib/systemd/systemd-journald").unwrap() };
        let config = guest_config(&image_with(&store, &[b"layer"], services, Some(journal)));
        assert_eq!(
            config.services,
            [
                ServiceConfig { name: "web".into(), argv: vec!["python".into(), "-m".into(), "http.server".into()] },
                ServiceConfig { name: "tick".into(), argv: vec!["sleep".into(), "1".into()] },
            ]
        );
        assert_eq!(config.journal, Some(JournalConfig { daemon: "/opt/env/lib/systemd/systemd-journald".into() }));
        let plain = guest_config(&image(&store, &[b"layer"]));
        assert!(plain.services.is_empty() && plain.journal.is_none());
    }

    #[test]
    fn pack_places_every_layer_at_its_aligned_offset() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap()).unwrap();
        let first = vec![0xAB; 5000];
        let second = vec![0xCD; 4096];
        let third = b"tiny".to_vec();
        let image = image(&store, &[&first, &second, &third]);

        let path = Packer.pack(&image, &store).unwrap();

        let bytes = fs::read(&path).unwrap();
        let header = PackHeader::<GuestConfig>::read_from(bytes.as_slice()).unwrap();
        assert_eq!(bytes.len() as u64, header.pack_len());
        assert_eq!(bytes.len() as u64 % PACK_ALIGN, 0);
        assert_eq!(header.layers().len(), 3);
        for (entry, (layer, expected)) in header.layers().iter().zip(image.layers().layers().iter().zip([&first, &second, &third])) {
            assert_eq!(entry.digest, layer.digest.to_bytes());
            assert_eq!(entry.offset % PACK_ALIGN, 0);
            let start = entry.offset as usize;
            assert_eq!(&bytes[start..start + entry.len as usize], expected.as_slice());
        }
        // Gaps between layers are zero padding.
        let end_of_first = (header.layers()[0].offset + header.layers()[0].len) as usize;
        assert!(bytes[end_of_first..header.layers()[1].offset as usize].iter().all(|&b| b == 0));
    }

    #[test]
    fn pack_rejects_a_blob_whose_size_differs_from_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let store = LayerStore::open(Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap()).unwrap();
        let image = image(&store, &[b"layer bytes"]);
        let layer = &image.layers().layers()[0];
        fs::write(store.blob_path(&layer.digest), b"truncated").unwrap();

        let err = Packer.pack(&image, &store).unwrap_err();
        assert!(matches!(err, PackerError::SizeMismatch { expected: 11, actual: 9, .. }), "{err}");
        assert!(!Packer.pack_path(&image, &store).exists());
    }

    #[test]
    fn run_pack_without_layers_is_a_readable_header() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let store = LayerStore::open(&root).unwrap();
        let target = root.join("run.img");

        Packer.pack_run(&[], RunConfig::default(), &store, &target).unwrap();

        let bytes = fs::read(&target).unwrap();
        assert_eq!(bytes.len() as u64, PACK_ALIGN);
        let header = PackHeader::<RunConfig>::read_from(bytes.as_slice()).unwrap();
        assert!(header.layers().is_empty());
        assert_eq!(header.config(), &RunConfig::default());
    }
}
