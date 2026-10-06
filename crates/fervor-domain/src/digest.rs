//! sha256-based identities. Each identity is its own type so a layer *key*
//! (derived from inputs) can never be confused with a layer *digest* (hash of
//! the built bytes).

use std::fmt;
use std::str::FromStr;

pub use rattler_digest::Sha256Hash;
use rattler_digest::Sha256;
use rattler_digest::digest::Digest;

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not a 64 character hex sha256 digest")]
pub struct DigestParseError(String);

macro_rules! sha256_identity {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Sha256Hash);

        impl $name {
            pub fn from_hash(hash: Sha256Hash) -> Self {
                Self(hash)
            }

            pub fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes.into())
            }

            /// The identity of `bytes`: their sha256.
            pub fn of_bytes(bytes: &[u8]) -> Self {
                Self(Sha256::digest(bytes))
            }

            pub fn as_hash(&self) -> &Sha256Hash {
                &self.0
            }

            pub fn to_bytes(self) -> [u8; 32] {
                self.0.into()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&hex::encode(self.0.as_slice()))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), hex::encode(self.0.as_slice()))
            }
        }

        impl FromStr for $name {
            type Err = DigestParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                rattler_digest::parse_digest_from_hex::<Sha256>(s)
                    .map(Self)
                    .ok_or_else(|| DigestParseError(s.to_owned()))
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

sha256_identity!(
    /// sha256 of a conda package archive, as published in repodata.
    PackageSha256
);
sha256_identity!(
    /// Input-addressed identity of a layer: computed *before* building, used
    /// as the build-cache lookup.
    LayerKey
);
sha256_identity!(
    /// Content-addressed identity of a built layer: sha256 of its SquashFS
    /// bytes. What gets transferred and verified.
    LayerDigest
);
sha256_identity!(
    /// sha256 of an image's canonical manifest.
    ImageId
);
sha256_identity!(
    /// sha256 of a pinned external artifact (kernel, hypervisor, init binary).
    ArtifactDigest
);
sha256_identity!(
    /// sha256 over a host directory tree (paths, modes, contents).
    TreeDigest
);

/// Length-prefixed, domain-separated sha256 encoder for identities derived
/// from structured inputs. Field boundaries are explicit, so `("ab", "c")` and
/// `("a", "bc")` never collide.
pub(crate) struct CanonicalHasher(Sha256);

impl CanonicalHasher {
    pub(crate) fn new(domain: &str) -> Self {
        let mut hasher = Self(Sha256::new());
        hasher.bytes(domain.as_bytes());
        hasher
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }

    pub(crate) fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }

    pub(crate) fn u64(&mut self, value: u64) -> &mut Self {
        self.0.update(value.to_le_bytes());
        self
    }

    pub(crate) fn finish(self) -> Sha256Hash {
        self.0.finalize()
    }
}
