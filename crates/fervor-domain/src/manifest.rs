//! `manifest.json`: the wire format of an [`Image`]. Kept separate from the
//! aggregate so either can evolve without silently changing the other.

use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};

use crate::boot::InitVersion;
use crate::entrypoint::Entrypoint;
use crate::image::{Image, ImageError, LayerStack, ScratchSize};
use crate::layer::BuiltLayer;
use crate::machine::ArtifactSet;
use crate::platform::GuestPlatform;
use crate::service::{Journal, Services};

pub const MANIFEST_SCHEMA: &str = "fervor.image/v1";
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestV1 {
    pub schema: String,
    pub platform: GuestPlatform,
    pub boot: BuiltLayer,
    /// Omitted for [`InitVersion::LEGACY`], so older manifests load and keep
    /// their id.
    #[serde(default, skip_serializing_if = "InitVersion::is_legacy")]
    pub init: InitVersion,
    /// Bottom → top.
    pub layers: Vec<BuiltLayer>,
    pub entrypoint: Entrypoint,
    /// Start order. Omitted when empty, so images without services keep
    /// their id and older manifests still load.
    #[serde(default, skip_serializing_if = "Services::is_empty")]
    pub services: Services,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal: Option<Journal>,
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
            init: image.init(),
            layers: image.layers().layers().to_vec(),
            entrypoint: image.entrypoint().clone(),
            services: image.services().clone(),
            journal: image.journal().cloned(),
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
            manifest.init,
            LayerStack::new(manifest.layers)?,
            manifest.entrypoint,
            manifest.services,
            manifest.journal,
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
    use crate::service::Service;
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
        image_with(top, Services::default(), None)
    }

    fn image_with(top: u8, services: Services, journal: Option<Journal>) -> Image {
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
            InitVersion::CURRENT,
            LayerStack::new(vec![layer(2, LayerKind::SmallGroup), layer(top, LayerKind::HostTree)]).unwrap(),
            entrypoint,
            services,
            journal,
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
    fn services_and_journal_round_trip_and_change_the_id() {
        let tick = Service {
            name: "tick".parse().unwrap(),
            argv: Argv::new(NonEmpty::new(vec!["sleep".into(), "1".into()]).unwrap()),
        };
        let journal = Journal { daemon: GuestPath::new("/opt/env/lib/systemd/systemd-journald").unwrap() };
        let original = image_with(3, Services::new(vec![tick]).unwrap(), Some(journal));
        let json = serde_json::to_string(&ManifestV1::from(&original)).unwrap();
        let parsed: Image = serde_json::from_str::<ManifestV1>(&json).unwrap().try_into().unwrap();
        assert_eq!(parsed, original);
        assert_ne!(original.id(), image(3).id());
    }

    #[test]
    fn manifests_without_services_omit_and_accept_missing_fields() {
        let json = serde_json::to_value(ManifestV1::from(&image(3))).unwrap();
        let fields = json.as_object().unwrap();
        assert!(!fields.contains_key("services") && !fields.contains_key("journal"));
        let parsed: Image = serde_json::from_value::<ManifestV1>(json.clone()).unwrap().try_into().unwrap();
        assert_eq!(parsed, image(3));

        // Manifests from before the init version was recorded.
        let mut old = json;
        old.as_object_mut().unwrap().remove("init");
        let parsed: Image = serde_json::from_value::<ManifestV1>(old).unwrap().try_into().unwrap();
        assert_eq!(parsed.init(), InitVersion::LEGACY);
        assert!(!serde_json::to_value(ManifestV1::from(&parsed)).unwrap().as_object().unwrap().contains_key("init"));
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
