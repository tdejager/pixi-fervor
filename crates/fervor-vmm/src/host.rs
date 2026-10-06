//! Runs images on the machine this process runs on.

use std::fs::OpenOptions;

use camino::Utf8PathBuf;
use fervor_domain::image::Image;
use fervor_domain::machine::{RunOutcome, RunRequest};
use fervor_store::LayerStore;

use crate::artifacts::ArtifactCache;
use crate::error::RunError;
use crate::pack::Packer;
use crate::vm::{VmFiles, VmSession};

/// The host's KVM device, which Firecracker opens read-write.
struct KvmDevice;

impl KvmDevice {
    const PATH: &str = "/dev/kvm";

    /// Fails with [`RunError::NoKvm`] unless this process can use KVM.
    fn check() -> Result<(), RunError> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(Self::PATH)
            .map(drop)
            .map_err(|e| RunError::NoKvm(e.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct LocalRunHost {
    pub store: LayerStore,
    /// Store root of the build host (e.g. mounted into this machine); missing
    /// layer blobs are imported from it.
    pub source_root: Option<Utf8PathBuf>,
    pub artifacts: ArtifactCache,
    pub packer: Packer,
}

impl LocalRunHost {
    /// Artifacts and packs share `store`'s root.
    pub fn new(store: LayerStore, source_root: Option<Utf8PathBuf>) -> Self {
        let artifacts = ArtifactCache::new(store.root());
        Self { store, source_root, artifacts, packer: Packer }
    }

    /// Imports the image's layers and writes its pack; blocking.
    fn pack(&self, image: &Image) -> Result<Utf8PathBuf, RunError> {
        self.import_layers(image)?;
        self.packer.pack(image, &self.store).map_err(|e| RunError::infrastructure("packing the image layers", e))
    }

    /// Makes every layer blob (boot layer included) present in the store.
    fn import_layers(&self, image: &Image) -> Result<(), RunError> {
        let store = &self.store;
        for layer in image.all_layers() {
            let digest = &layer.digest;
            if store.contains(digest).map_err(|e| RunError::infrastructure("checking the layer store", e))? {
                continue;
            }
            // Both stores share the same layout, so the blob's path relative to
            // our root is its path in the source store.
            let relative = store.blob_path(digest);
            let relative = relative.strip_prefix(store.root()).expect("blob paths live under the store root");
            match &self.source_root {
                Some(source_root) if source_root.join(relative).is_file() => store
                    .import_from(source_root, digest)
                    .map_err(|e| RunError::infrastructure(format!("importing layer {digest} from {source_root}"), e))?,
                _ => return Err(RunError::MissingLayer(*digest)),
            }
        }
        Ok(())
    }

    /// Makes the image's layers available on this host (importing missing
    /// digests from `source_root`), boots it and waits until the guest is gone.
    pub async fn run(&self, image: &Image, request: &RunRequest) -> Result<RunOutcome, RunError> {
        KvmDevice::check()?;

        let (host, image_owned) = (self.clone(), image.clone());
        let pack = tokio::task::spawn_blocking(move || host.pack(&image_owned))
            .await
            .map_err(|e| RunError::infrastructure("packing the image layers", e))??;

        let pinned = image.artifacts();
        let firecracker = self
            .artifacts
            .ensure(&pinned.firecracker)
            .await
            .map_err(|e| RunError::infrastructure("fetching firecracker", e))?;
        let kernel = self
            .artifacts
            .ensure(&pinned.kernel.artifact)
            .await
            .map_err(|e| RunError::infrastructure("fetching the guest kernel", e))?;

        let boot_layer = self.store.blob_path(&image.boot().digest);
        let files = VmFiles { firecracker: &firecracker, kernel: &kernel, boot_layer: &boot_layer, pack: &pack };
        VmSession::prepare(files, request).await?.run().await
    }
}
