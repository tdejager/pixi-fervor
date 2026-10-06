//! `manifest.json`: the wire format of an [`Image`]. Kept separate from the
//! aggregate so either can evolve without silently changing the other.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::entrypoint::Entrypoint;
use crate::image::{Image, ImageError, LayerStack, ScratchSize};
use crate::layer::BuiltLayer;
use crate::machine::ArtifactSet;
use crate::platform::GuestPlatform;

pub const MANIFEST_SCHEMA: &str = "fervor.image/v1";
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestV1 {
    pub schema: String,
    pub platform: GuestPlatform,
    pub boot: BuiltLayer,
    /// Bottom → top.
    pub layers: Vec<BuiltLayer>,
    pub entrypoint: Entrypoint,
    pub scratch_size_mib: NonZeroU32,
    pub artifacts: ArtifactSet,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("unsupported manifest schema `{0}`, expected `{MANIFEST_SCHEMA}`")]
    UnsupportedSchema(String),
    #[error(transparent)]
    Image(#[from] ImageError),
}

impl From<&Image> for ManifestV1 {
    fn from(image: &Image) -> Self {
        Self {
            schema: MANIFEST_SCHEMA.to_owned(),
            platform: image.platform(),
            boot: image.boot().clone(),
            layers: image.layers().layers().to_vec(),
            entrypoint: image.entrypoint().clone(),
            scratch_size_mib: image.scratch().0,
            artifacts: image.artifacts().clone(),
        }
    }
}

impl TryFrom<ManifestV1> for Image {
    type Error = ManifestError;

    fn try_from(manifest: ManifestV1) -> Result<Self, Self::Error> {
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(ManifestError::UnsupportedSchema(manifest.schema));
        }
        Ok(Image::new(
            manifest.platform,
            manifest.boot,
            LayerStack::new(manifest.layers)?,
            manifest.entrypoint,
            ScratchSize(manifest.scratch_size_mib),
            manifest.artifacts,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::{ArtifactDigest, LayerDigest, LayerKey};
    use crate::entrypoint::{Argv, EnvVars};
    use crate::layer::{EnvPrefix, LayerKind};
    use crate::machine::{KernelArtifact, PinnedArtifact};
    use crate::nonempty::NonEmpty;
    use crate::path::GuestPath;
    use crate::platform::KernelFormat;
    use crate::size::ByteSize;

    fn layer(byte: u8, kind: LayerKind) -> BuiltLayer {
        BuiltLayer {
            key: LayerKey::from_bytes([byte; 32]),
            digest: LayerDigest::from_bytes([byte + 100; 32]),
            size: ByteSize::bytes(4096),
            kind,
            label: format!("layer {byte}"),
        }
    }

    fn image(top: u8) -> Image {
        let argv = Argv::new(NonEmpty::new(vec!["python".into(), "app.py".into()]).unwrap());
        let entrypoint = Entrypoint::new(argv, EnvVars::default(), GuestPath::new("/app").unwrap(), &EnvPrefix::default());
        let artifact = |name: &str| PinnedArtifact {
            url: format!("https://example.com/{name}").parse().unwrap(),
            member: None,
            sha256: ArtifactDigest::from_bytes([1; 32]),
        };
        Image::new(
            GuestPlatform::LinuxAarch64,
            layer(1, LayerKind::Boot),
            LayerStack::new(vec![layer(2, LayerKind::SmallGroup), layer(top, LayerKind::HostTree)]).unwrap(),
            entrypoint,
            ScratchSize::default(),
            ArtifactSet {
                firecracker: artifact("firecracker"),
                kernel: KernelArtifact {
                    artifact: artifact("kernel"),
                    format: KernelFormat::PeImage,
                    version: "6.18.51".to_owned(),
                },
            },
        )
        .unwrap()
    }

    #[test]
    fn manifest_round_trips_and_identifies_the_image() {
        let original = image(3);
        let json = serde_json::to_string(&ManifestV1::from(&original)).unwrap();
        let parsed: Image = serde_json::from_str::<ManifestV1>(&json).unwrap().try_into().unwrap();
        assert_eq!(parsed, original);
        assert_eq!(parsed.id(), original.id());
        assert_ne!(image(4).id(), original.id());
    }

    #[test]
    fn rejects_unknown_schema_and_misplaced_boot_layers() {
        let mut manifest = ManifestV1::from(&image(3));
        manifest.schema = "fervor.image/v0".into();
        assert!(matches!(Image::try_from(manifest), Err(ManifestError::UnsupportedSchema(_))));

        let mut manifest = ManifestV1::from(&image(3));
        manifest.layers.push(layer(9, LayerKind::Boot));
        assert!(Image::try_from(manifest).is_err());
    }
}
