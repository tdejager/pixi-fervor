//! Pinned external artifacts (Firecracker, guest kernel), stored by sha256 at
//! `<root>/artifacts/<sha256>`.

use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::str::FromStr;

use camino::Utf8PathBuf;
use fervor_domain::digest::ArtifactDigest;
use fervor_domain::machine::{ArtifactSet, KernelArtifact, PinnedArtifact};
use fervor_domain::platform::{GuestPlatform, KernelFormat, UnsupportedPlatform};
use flate2::read::GzDecoder;
use rattler_digest::Sha256;
use tempfile::NamedTempFile;
use url::Url;

/// Pinned Firecracker and guest kernel releases for the runtime platform.
pub fn pinned_artifacts(platform: GuestPlatform) -> Result<ArtifactSet, UnsupportedPlatform> {
    let url = |s: &str| Url::parse(s).expect("valid pinned url");
    let digest = |s: &str| ArtifactDigest::from_str(s).expect("valid pinned digest");
    match platform {
        GuestPlatform::LinuxAarch64 => Ok(ArtifactSet {
            firecracker: PinnedArtifact {
                url: url("https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-aarch64.tgz"),
                member: Some("release-v1.17.0-aarch64/firecracker-v1.17.0-aarch64".to_owned()),
                sha256: digest("fe726e0b43c04363ac07e358be4dee982c3947c65ed3ae10c770fef5e1cd756c"),
            },
            kernel: KernelArtifact {
                artifact: PinnedArtifact {
                    url: url("https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/20260930-a738f18a8db0-0/aarch64/vmlinux-6.18.51"),
                    member: None,
                    sha256: digest("eb69a8656e1217b590ff0caa00973909ffa9327ab5db859634c83fb9a4da17c3"),
                },
                format: KernelFormat::PeImage,
                version: "6.18.51".to_owned(),
            },
        }),
        // No x86_64 pins yet: nothing in the POC boots there.
        GuestPlatform::LinuxX86_64 => Err(UnsupportedPlatform(platform.to_string())),
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactCache {
    root: Utf8PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("downloading {url}")]
    Download {
        url: Url,
        #[source]
        source: reqwest::Error,
    },
    #[error("{url} does not contain `{member}`")]
    MissingMember { url: Url, member: String },
    #[error("artifact from {url} hashes to {actual}, expected {expected}")]
    Checksum { url: Box<Url>, expected: ArtifactDigest, actual: ArtifactDigest },
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl ArtifactError {
    fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io { context: context.into(), source }
    }
}

impl ArtifactCache {
    /// `root` is the shared fervor cache root (the layer store's root).
    pub fn new(root: impl Into<Utf8PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, artifact: &PinnedArtifact) -> Utf8PathBuf {
        self.root.join("artifacts").join(artifact.sha256.to_string())
    }

    /// Returns the verified, executable artifact, downloading it first unless
    /// it is already cached.
    pub async fn ensure(&self, artifact: &PinnedArtifact) -> Result<Utf8PathBuf, ArtifactError> {
        let target = self.path(artifact);
        if target.try_exists().map_err(|e| ArtifactError::io(format!("checking {target}"), e))? {
            return Ok(target);
        }
        let tmp_dir = self.root.join("tmp");
        for dir in [self.root.join("artifacts"), tmp_dir.clone()] {
            fs::create_dir_all(&dir).map_err(|e| ArtifactError::io(format!("creating {dir}"), e))?;
        }

        let download = ArtifactDownload { artifact: artifact.clone(), tmp_dir, target: target.clone() };
        let staged = download.fetch().await?;
        tokio::task::spawn_blocking(move || download.finish(staged))
            .await
            .map_err(|e| ArtifactError::io("finishing artifact download", io::Error::other(e)))??;
        Ok(target)
    }
}

/// One artifact on its way into the cache: staged under `tmp/`, published at
/// `target` once verified.
struct ArtifactDownload {
    artifact: PinnedArtifact,
    tmp_dir: Utf8PathBuf,
    target: Utf8PathBuf,
}

impl ArtifactDownload {
    /// Downloads the artifact's URL into a staged file.
    async fn fetch(&self) -> Result<NamedTempFile, ArtifactError> {
        let url = &self.artifact.url;
        let staged = NamedTempFile::new_in(&self.tmp_dir).map_err(|e| ArtifactError::io("staging download", e))?;
        tracing::info!(%url, "downloading artifact");

        let download_err = |source| ArtifactError::Download { url: url.clone(), source };
        let client = reqwest::Client::builder().build().map_err(download_err)?;
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(download_err)?;
        let mut out = io::BufWriter::new(staged.as_file());
        while let Some(chunk) = response.chunk().await.map_err(download_err)? {
            out.write_all(&chunk).map_err(|e| ArtifactError::io(format!("writing download of {url}"), e))?;
        }
        out.flush().map_err(|e| ArtifactError::io(format!("writing download of {url}"), e))?;
        drop(out);
        Ok(staged)
    }

    /// Extracts the member (if any), verifies, marks executable and publishes.
    fn finish(&self, download: NamedTempFile) -> Result<(), ArtifactError> {
        let artifact = &self.artifact;
        let file = match &artifact.member {
            None => download,
            Some(member) => {
                let extracted = NamedTempFile::new_in(&self.tmp_dir).map_err(|e| ArtifactError::io("staging extraction", e))?;
                self.extract_member(download.path(), member, extracted.as_file())?;
                extracted
            }
        };

        let actual = rattler_digest::compute_file_digest::<Sha256>(file.path())
            .map(ArtifactDigest::from_hash)
            .map_err(|e| ArtifactError::io(format!("hashing download of {}", artifact.url), e))?;
        if actual != artifact.sha256 {
            return Err(ArtifactError::Checksum { url: Box::new(artifact.url.clone()), expected: artifact.sha256, actual });
        }

        let target = &self.target;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .map_err(|e| ArtifactError::io(format!("marking {target} executable"), e))?;
        file.persist(target).map_err(|e| ArtifactError::io(format!("storing {target}"), e.error))?;
        Ok(())
    }

    fn extract_member(&self, tgz: &Path, member: &str, mut out: &File) -> Result<(), ArtifactError> {
        let url = &self.artifact.url;
        let context = || format!("reading {url} as a gzip'd tarball");
        let file = File::open(tgz).map_err(|e| ArtifactError::io(context(), e))?;
        let mut archive = tar::Archive::new(GzDecoder::new(file));
        for entry in archive.entries().map_err(|e| ArtifactError::io(context(), e))? {
            let mut entry = entry.map_err(|e| ArtifactError::io(context(), e))?;
            let path = entry.path().map_err(|e| ArtifactError::io(context(), e))?;
            let path = path.strip_prefix("./").unwrap_or(&path);
            if path == Path::new(member) {
                io::copy(&mut entry, &mut out).map_err(|e| ArtifactError::io(format!("extracting {member} from {url}"), e))?;
                return Ok(());
            }
        }
        Err(ArtifactError::MissingMember { url: url.clone(), member: member.to_owned() })
    }
}
