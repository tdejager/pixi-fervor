//! Build an image: resolve → plan layers → reuse or build each layer →
//! check path conflicts → assemble the image → save its manifest.

use std::time::{Duration, Instant};

use camino::{Utf8Path, Utf8PathBuf};
use fervor_conda::{ContentsError, CondaClient, InstallTarget, RattlerPackageContents, RattlerResolver, ResolveError};
use fervor_domain::boot::{BootLayerSpec, InitBinary};
use fervor_domain::entrypoint::{Argv, Entrypoint, EnvVars};
use fervor_domain::environment::{EnvironmentSpec, ResolvedEnvironment};
use fervor_domain::image::{Image, ImageError, LayerStack, ScratchSize};
use fervor_domain::layer::{
    BuiltLayer, ConflictPolicy, EnvPrefix, LayerKind, LayerPlan, LayerSource, LayoutVersion, PathConflicts,
    PlannedLayer, SizeThresholdPolicy, StoredLayer,
};
use fervor_domain::machine::ArtifactSet;
use fervor_domain::path::GuestPath;
use fervor_layerfs::{BuildError, HostTreeError, HostTreeReader, SquashfsLayerBuilder};
use fervor_store::{ImageDir, ImageDirError, LayerStore, StoreError};
use futures::{StreamExt, TryStreamExt};

/// A host directory copied into the image.
#[derive(Debug, Clone)]
pub struct HostTreeMount {
    pub source: Utf8PathBuf,
    pub mount_at: GuestPath,
}

#[derive(Debug, Clone)]
pub struct BuildImage {
    pub environment: EnvironmentSpec,
    pub artifacts: ArtifactSet,
    pub argv: Argv,
    pub env: EnvVars,
    pub workdir: GuestPath,
    /// Placed on top of the package layers, in order.
    pub host_trees: Vec<HostTreeMount>,
    pub policy: SizeThresholdPolicy,
    pub conflicts: ConflictPolicy,
    pub scratch: ScratchSize,
    pub output: Utf8PathBuf,
}

/// Progress of a build, reported as it happens.
#[derive(Debug, Clone, Copy)]
pub enum BuildEvent<'a> {
    Resolved(&'a ResolvedEnvironment),
    Planned(&'a LayerPlan),
    LayerReused(&'a BuiltLayer),
    LayerBuilt { layer: &'a BuiltLayer, elapsed: Duration },
}

#[derive(Debug, thiserror::Error)]
pub enum BuildImageError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Contents(#[from] ContentsError),
    #[error(transparent)]
    HostTree(#[from] HostTreeError),
    #[error(transparent)]
    Build(#[from] BuildError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    ImageDir(#[from] ImageDirError),
    #[error(transparent)]
    Image(#[from] ImageError),
    #[error("{0}")]
    Conflicts(PathConflicts),
}

/// Builds images from conda packages and host directories into a layer store.
pub struct ImageBuilder {
    resolver: RattlerResolver,
    contents: RattlerPackageContents,
    layers: SquashfsLayerBuilder,
    store: LayerStore,
    init: InitBinary,
    prefix: EnvPrefix,
    layout: LayoutVersion,
}

impl ImageBuilder {
    /// Layers fetched and built concurrently.
    const CONCURRENCY: usize = 4;

    /// Everything lives under `cache_root`: layers, package archives, repodata.
    pub fn new(cache_root: &Utf8Path, client: CondaClient, init: InitBinary) -> Result<Self, StoreError> {
        let store = LayerStore::open(cache_root)?;
        Ok(Self {
            resolver: RattlerResolver::new(cache_root, client.clone()),
            contents: RattlerPackageContents::new(cache_root, client),
            layers: SquashfsLayerBuilder::new(store.tmp_dir()),
            store,
            init,
            prefix: EnvPrefix::default(),
            layout: LayoutVersion::CURRENT,
        })
    }

    /// Must run on a multi-threaded tokio runtime: layer writing is CPU-bound
    /// and runs via `block_in_place`.
    pub async fn build(&self, command: BuildImage, progress: &dyn Fn(BuildEvent<'_>)) -> Result<Image, BuildImageError> {
        let env = self.resolver.resolve(&command.environment).await?;
        progress(BuildEvent::Resolved(&env));

        let mut plan = command.policy.plan(&env, &self.prefix, self.layout);
        for mount in &command.host_trees {
            plan.push_host_tree(HostTreeReader::snapshot(&mount.source, &mount.mount_at)?, self.layout);
        }
        progress(BuildEvent::Planned(&plan));

        let boot = self.boot_layer(&env, progress).await?;
        let target = InstallTarget { prefix: self.prefix.clone(), platform: env.platform(), python: env.python() };
        let stack: Vec<StoredLayer> = futures::stream::iter(plan.layers())
            .map(|layer| self.layer(layer, &target, progress))
            .buffered(Self::CONCURRENCY)
            .try_collect()
            .await?;

        let conflicts = PathConflicts::detect(std::iter::once(&boot.index).chain(stack.iter().map(|s| &s.index)));
        if !command.conflicts.admits(&conflicts) {
            return Err(BuildImageError::Conflicts(conflicts));
        }

        let entrypoint = Entrypoint::new(command.argv, command.env, command.workdir, &self.prefix);
        let layers = LayerStack::new(stack.into_iter().map(|stored| stored.layer).collect())?;
        let image = Image::new(env.platform(), boot.layer, layers, entrypoint, command.scratch, command.artifacts)?;
        ImageDir::new(command.output).save(&image)?;
        Ok(image)
    }

    async fn boot_layer(&self, env: &ResolvedEnvironment, progress: &dyn Fn(BuildEvent<'_>)) -> Result<StoredLayer, BuildImageError> {
        let spec = BootLayerSpec { platform: env.platform(), sysroot: env.sysroot().clone(), init: self.init.clone() };
        let key = spec.key(self.layout);
        if let Some(stored) = self.store.lookup(&key)? {
            progress(BuildEvent::LayerReused(&stored.layer));
            return Ok(stored);
        }
        let started = Instant::now();
        let files = spec.files(self.contents.archive_entries(&spec.sysroot).await?);
        let blob = tokio::task::block_in_place(|| self.layers.build(&[files]))?;
        let stored = self.store.insert(key, LayerKind::Boot, spec.label(), blob)?;
        progress(BuildEvent::LayerBuilt { layer: &stored.layer, elapsed: started.elapsed() });
        Ok(stored)
    }

    async fn layer(&self, layer: &PlannedLayer, target: &InstallTarget, progress: &dyn Fn(BuildEvent<'_>)) -> Result<StoredLayer, BuildImageError> {
        if let Some(stored) = self.store.lookup(&layer.key())? {
            progress(BuildEvent::LayerReused(&stored.layer));
            return Ok(stored);
        }
        let started = Instant::now();
        let sets = match layer.source() {
            LayerSource::Packages(members) => {
                futures::future::try_join_all(members.iter().map(|p| self.contents.installed_files(p, target))).await?
            }
            LayerSource::HostTree(snapshot) => vec![snapshot.files.clone()],
        };
        let blob = tokio::task::block_in_place(|| self.layers.build(&sets))?;
        let stored = self.store.insert(layer.key(), layer.kind(), layer.label().to_owned(), blob)?;
        progress(BuildEvent::LayerBuilt { layer: &stored.layer, elapsed: started.elapsed() });
        Ok(stored)
    }
}
