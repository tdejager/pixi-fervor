//! Verified package archive cache at `<root>/pkgs/<sha256>-<file name>`.
//!
//! Archives are downloaded into a temporary file next to their final
//! location, hashed while streaming and only renamed into place once the
//! sha256 matches, so a file under its final name is always verified.
//! Concurrent fetches of the same package race harmlessly: each writes its
//! own temporary file and the renames are atomic.

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::environment::ResolvedPackage;
use futures::StreamExt;
use rattler_digest::Sha256;
use rattler_digest::digest::Digest;
use reqwest_middleware::ClientWithMiddleware;
use tokio::io::AsyncWriteExt;

use crate::error::ContentsError;

#[derive(Clone)]
pub struct ArchiveCache {
    dir: Utf8PathBuf,
    client: ClientWithMiddleware,
}

impl ArchiveCache {
    pub fn new(cache_root: &Utf8Path, client: ClientWithMiddleware) -> Self {
        Self { dir: cache_root.join("pkgs"), client }
    }

    /// Path of the verified archive of `package`, downloading it first if
    /// it is not cached yet.
    pub async fn fetch(&self, package: &ResolvedPackage) -> Result<Utf8PathBuf, ContentsError> {
        let record = package.record();
        let path = self.dir.join(format!("{}-{}", package.sha256(), record.identifier.to_file_name()));
        if tokio::fs::try_exists(&path)
            .await
            .map_err(|e| Self::failure(package, "failed to inspect the package cache", e))?
        {
            return Ok(path);
        }
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| Self::failure(package, "failed to create the package cache", e))?;
        let partial = tempfile::Builder::new()
            .prefix(".download-")
            .tempfile_in(&self.dir)
            .map_err(|e| Self::failure(package, "failed to create a download file", e))?;

        let digest = if record.url.scheme() == "file" {
            let source = record
                .url
                .to_file_path()
                .map_err(|()| Self::failure(package, "invalid file url", std::io::Error::other(record.url.to_string())))?;
            let destination =
                partial.reopen().map_err(|e| Self::failure(package, "failed to open the download file", e))?;
            tokio::task::spawn_blocking(move || Self::copy_hashing(&source, destination))
                .await
                .map_err(|e| Self::failure(package, "copy task panicked", e))?
                .map_err(|e| Self::failure(package, "failed to copy the package archive", e))?
        } else {
            self.download(package, &partial).await?
        };

        if digest.as_slice() != package.sha256().as_hash().as_slice() {
            return Err(ContentsError::DigestMismatch { package: package.display_name() });
        }
        partial
            .persist(&path)
            .map_err(|e| Self::failure(package, "failed to move the archive into the package cache", e.error))?;
        Ok(path)
    }

    async fn download(
        &self,
        package: &ResolvedPackage,
        partial: &tempfile::NamedTempFile,
    ) -> Result<rattler_digest::Sha256Hash, ContentsError> {
        let url = package.record().url.clone();
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| Self::failure(package, "failed to download the package archive", e))?
            .error_for_status()
            .map_err(|e| Self::failure(package, "failed to download the package archive", e))?;
        let file = partial.reopen().map_err(|e| Self::failure(package, "failed to open the download file", e))?;
        let mut file = tokio::fs::File::from_std(file);
        let mut hasher = Sha256::new();
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| Self::failure(package, "failed to download the package archive", e))?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| Self::failure(package, "failed to write the package archive", e))?;
        }
        file.flush().await.map_err(|e| Self::failure(package, "failed to write the package archive", e))?;
        Ok(hasher.finalize())
    }

    /// Copies a local (`file://`) archive into the download file, hashing it
    /// on the way.
    fn copy_hashing(
        source: &std::path::Path,
        destination: std::fs::File,
    ) -> std::io::Result<rattler_digest::Sha256Hash> {
        let mut writer = rattler_digest::HashingWriter::<_, Sha256>::new(destination);
        std::io::copy(&mut std::fs::File::open(source)?, &mut writer)?;
        let (_, digest) = writer.finalize();
        Ok(digest)
    }

    /// Infrastructure failure `what` while handling `package`.
    pub(crate) fn failure(
        package: &ResolvedPackage,
        what: &str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> ContentsError {
        ContentsError::infrastructure(format!("{what} for `{}`", package.display_name()), source)
    }
}
