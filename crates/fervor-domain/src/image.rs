//! Image context: the aggregate root that is built, transferred and booted.

use std::num::NonZeroU32;

use crate::digest::ImageId;
use crate::entrypoint::Entrypoint;
use crate::layer::{BuiltLayer, LayerKind};
use crate::machine::ArtifactSet;
use crate::manifest::ManifestV1;
use crate::platform::GuestPlatform;

/// overlayfs refuses stacks deeper than this many lower layers.
pub const MAX_LOWER_LAYERS: usize = 500;

/// Size limit of the tmpfs that backs the guest's writable overlay layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScratchSize(pub NonZeroU32);

impl ScratchSize {
    pub fn mib(self) -> u32 {
        self.0.get()
    }
}

impl Default for ScratchSize {
    fn default() -> Self {
        Self(NonZeroU32::new(1024).expect("non-zero"))
    }
}

/// Layers above the boot layer, bottom → top.
///
/// Invariants: no boot layers, and together with the boot layer the stack
/// fits overlayfs' lower-layer limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerStack(Vec<BuiltLayer>);

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("layer `{0}` is a boot layer and cannot be part of the layer stack")]
    BootLayerInStack(String),
    #[error("`{0}` is not a boot layer")]
    NotABootLayer(String),
    #[error("{0} layers exceed overlayfs' limit of {MAX_LOWER_LAYERS} (including the boot layer)")]
    TooManyLayers(usize),
}

impl LayerStack {
    pub fn new(layers: Vec<BuiltLayer>) -> Result<Self, ImageError> {
        if let Some(boot) = layers.iter().find(|l| l.kind == LayerKind::Boot) {
            return Err(ImageError::BootLayerInStack(boot.label.clone()));
        }
        if layers.len() + 1 > MAX_LOWER_LAYERS {
            return Err(ImageError::TooManyLayers(layers.len() + 1));
        }
        Ok(Self(layers))
    }

    pub fn layers(&self) -> &[BuiltLayer] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    platform: GuestPlatform,
    boot: BuiltLayer,
    layers: LayerStack,
    entrypoint: Entrypoint,
    scratch: ScratchSize,
    artifacts: ArtifactSet,
}

impl Image {
    pub fn new(
        platform: GuestPlatform,
        boot: BuiltLayer,
        layers: LayerStack,
        entrypoint: Entrypoint,
        scratch: ScratchSize,
        artifacts: ArtifactSet,
    ) -> Result<Self, ImageError> {
        if boot.kind != LayerKind::Boot {
            return Err(ImageError::NotABootLayer(boot.label));
        }
        Ok(Self { platform, boot, layers, entrypoint, scratch, artifacts })
    }

    /// sha256 of the canonical manifest.
    pub fn id(&self) -> ImageId {
        let manifest = serde_json::to_vec(&ManifestV1::from(self)).expect("manifest serializes");
        ImageId::of_bytes(&manifest)
    }

    pub fn platform(&self) -> GuestPlatform {
        self.platform
    }

    pub fn boot(&self) -> &BuiltLayer {
        &self.boot
    }

    pub fn layers(&self) -> &LayerStack {
        &self.layers
    }

    pub fn entrypoint(&self) -> &Entrypoint {
        &self.entrypoint
    }

    pub fn scratch(&self) -> ScratchSize {
        self.scratch
    }

    pub fn artifacts(&self) -> &ArtifactSet {
        &self.artifacts
    }

    /// Boot layer first, then the stack bottom → top.
    pub fn all_layers(&self) -> impl Iterator<Item = &BuiltLayer> {
        std::iter::once(&self.boot).chain(self.layers.layers())
    }
}
