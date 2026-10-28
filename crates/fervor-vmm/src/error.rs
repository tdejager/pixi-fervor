use fervor_domain::digest::LayerDigest;
use fervor_domain::platform::GuestPlatform;

/// Underlying failure carried by [`RunError::Infrastructure`].
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("run host has no usable /dev/kvm: {0}")]
    NoKvm(String),
    #[error("the image is for {image}, but the run host is {host_arch}; KVM only boots guests of the host's architecture")]
    PlatformMismatch { image: GuestPlatform, host_arch: &'static str },
    #[error("layer {0} is not available on the run host and cannot be transferred")]
    MissingLayer(LayerDigest),
    #[error("the image was built by an older fervor whose guest init ignores {0}; rebuild the image")]
    InitTooOld(&'static str),
    #[error("--network nat needs root with CAP_NET_ADMIN on the run host")]
    NoNetAdmin,
    #[error("{context}")]
    Infrastructure {
        context: String,
        #[source]
        source: BoxError,
    },
}

impl RunError {
    pub fn infrastructure(context: impl Into<String>, source: impl Into<BoxError>) -> Self {
        Self::Infrastructure { context: context.into(), source: source.into() }
    }
}
