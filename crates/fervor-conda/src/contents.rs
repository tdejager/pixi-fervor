use camino::Utf8Path;
use fervor_domain::environment::ResolvedPackage;
use fervor_domain::files::{ArchiveEntry, FileSet};

use crate::archive::PackageArchive;
use crate::client::CondaClient;
use crate::download::ArchiveCache;
use crate::error::ContentsError;
use crate::install::{InstallError, InstallTarget, PackageInstall};

/// Reads conda package archives, downloading each one once into
/// `<cache root>/pkgs/` and verifying its sha256. Archives are decoded in
/// memory; package files are never written to the host filesystem.
///
/// Cheap to clone; safe to use concurrently for different packages.
#[derive(Clone)]
pub struct RattlerPackageContents {
    cache: ArchiveCache,
}

impl RattlerPackageContents {
    pub fn new(cache_root: &Utf8Path, client: CondaClient) -> Self {
        Self { cache: ArchiveCache::new(cache_root, client.http().clone()) }
    }

    async fn read_archive(&self, package: &ResolvedPackage) -> Result<PackageArchive, ContentsError> {
        let path = self.cache.fetch(package).await?;
        tokio::task::spawn_blocking(move || PackageArchive::read(&path))
            .await
            .map_err(|e| ArchiveCache::failure(package, "archive reader panicked", e))?
            .map_err(|e| ArchiveCache::failure(package, "failed to read the package archive", e))
    }

    /// Files of `package` as they appear inside `target.prefix`: prefix
    /// placeholders replaced, `noarch: python` files relocated, entry points
    /// generated, `conda-meta/<package>.json` written.
    pub async fn installed_files(&self, package: &ResolvedPackage, target: &InstallTarget) -> Result<FileSet, ContentsError> {
        let archive = self.read_archive(package).await?;
        let record = package.record().clone();
        let install_target = target.clone();
        tokio::task::spawn_blocking(move || PackageInstall::new(&record, archive, &install_target).files())
            .await
            .map_err(|e| ArchiveCache::failure(package, "install task panicked", e))?
            .map_err(|error| match error {
                InstallError::MissingPython => ContentsError::MissingPython { package: package.display_name() },
                other => ArchiveCache::failure(package, "failed to install the package", other),
            })
    }

    /// Entries exactly as stored in the archive (paths relative to its root).
    pub async fn archive_entries(&self, package: &ResolvedPackage) -> Result<Vec<ArchiveEntry>, ContentsError> {
        let archive = self.read_archive(package).await?;
        Ok(archive.content.into_iter().map(|(path, kind)| ArchiveEntry { path, kind }).collect())
    }
}
