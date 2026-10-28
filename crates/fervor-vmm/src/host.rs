//! Runs images on the machine this process runs on.

use std::fs::OpenOptions;

use camino::Utf8PathBuf;
use fervor_domain::boot::InitVersion;
use fervor_domain::digest::LayerDigest;
use fervor_domain::image::Image;
use fervor_domain::machine::{NetworkMode, RunLayer, RunOutcome, RunRequest};
use fervor_domain::platform::GuestPlatform;
use fervor_domain::size::ByteSize;
use fervor_guest_abi::RunConfig;
use fervor_store::LayerStore;

use crate::artifacts::ArtifactCache;
use crate::error::RunError;
use crate::network::{self, NatNetwork};
use crate::pack::Packer;
use crate::vm::{RunDir, Signals, VmFiles, VmSession};

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

    /// Imports the image's and the run's layers and writes the image pack
    /// (unless cached); blocking.
    fn pack_image(&self, image: &Image, run_layers: &[RunLayer]) -> Result<Utf8PathBuf, RunError> {
        for digest in image.all_layers().map(|l| &l.digest).chain(run_layers.iter().map(|l| &l.digest)) {
            self.import(digest)?;
        }
        self.packer.pack(image, &self.store).map_err(|e| RunError::infrastructure("packing the image layers", e))
    }

    /// Makes the blob `digest` present in the store.
    fn import(&self, digest: &LayerDigest) -> Result<(), RunError> {
        let store = &self.store;
        if store.contains(digest).map_err(|e| RunError::infrastructure("checking the layer store", e))? {
            return Ok(());
        }
        // Both stores share the same layout, so the blob's path relative to
        // our root is its path in the source store.
        let relative = store.blob_path(digest);
        let relative = relative.strip_prefix(store.root()).expect("blob paths live under the store root");
        match &self.source_root {
            Some(source_root) if source_root.join(relative).is_file() => store
                .import_from(source_root, digest)
                .map_err(|e| RunError::infrastructure(format!("importing layer {digest} from {source_root}"), e)),
            _ => Err(RunError::MissingLayer(*digest)),
        }
    }

    /// The run layer whose blob is `digest` (e.g. `fervor-runner
    /// --run-layer`), imported from `source_root` if missing; blocking.
    pub fn run_layer(&self, digest: LayerDigest) -> Result<RunLayer, RunError> {
        self.import(&digest)?;
        let blob = self.store.blob_path(&digest);
        let size = std::fs::metadata(&blob).map_err(|e| RunError::infrastructure(format!("reading {blob}"), e))?.len();
        Ok(RunLayer { digest, size: ByteSize::bytes(size) })
    }

    /// Makes the image's and the run's layers available on this host
    /// (importing missing digests from `source_root`), sets up the network,
    /// boots the image and waits until the guest is gone. Whatever was set up
    /// is removed again on every way out, signals included.
    pub async fn run(&self, image: &Image, request: &RunRequest) -> Result<RunOutcome, RunError> {
        if GuestPlatform::native() != Some(image.platform()) {
            return Err(RunError::PlatformMismatch { image: image.platform(), host_arch: std::env::consts::ARCH });
        }
        KvmDevice::check()?;
        if image.init() < InitVersion::RUN_PACK {
            if request.network != NetworkMode::None {
                return Err(RunError::InitTooOld("--network"));
            }
            if !request.layers.is_empty() {
                return Err(RunError::InitTooOld("run-time files (--copy/--file)"));
            }
        }
        if request.network == NetworkMode::Nat {
            NatNetwork::check_permission()?;
        }

        // The slow steps (imports, packing, downloads) leave nothing behind
        // that needs cleaning up, so they run before the signal handlers
        // exist: a signal simply ends the process.
        let (host, image_owned, run_layers) = (self.clone(), image.clone(), request.layers.clone());
        let pack = tokio::task::spawn_blocking(move || host.pack_image(&image_owned, &run_layers))
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

        // From here on a signal is handled (pending ones are seen by
        // `VmSession::run`), so the network and run directory are torn down.
        let signals = Signals::install()?;
        let run_dir = RunDir::create()?;
        let network = match request.network {
            NetworkMode::None => None,
            NetworkMode::Nat => Some(
                tokio::task::spawn_blocking(NatNetwork::setup)
                    .await
                    .map_err(|e| RunError::infrastructure("setting up the network", e))??,
            ),
        };
        let run_config = RunConfig { network: network.as_ref().map(|n| n.guest_config(network::host_resolv_conf())) };
        let (packer, store, run_layers, run_pack) = (self.packer, self.store.clone(), request.layers.clone(), run_dir.paths().run_pack());
        tokio::task::spawn_blocking(move || packer.pack_run(&run_layers, run_config, &store, &run_pack))
            .await
            .map_err(|e| RunError::infrastructure("packing the run layers", e))?
            .map_err(|e| RunError::infrastructure("packing the run layers", e))?;

        let boot_layer = self.store.blob_path(&image.boot().digest);
        let files = VmFiles { firecracker: &firecracker, kernel: &kernel, boot_layer: &boot_layer, pack: &pack };
        let interface = network.as_ref().map(NatNetwork::interface);
        // `network` lives until the VM is gone; dropping it tears it down.
        VmSession::prepare(signals, run_dir, files, interface, request).await?.run().await
    }
}
