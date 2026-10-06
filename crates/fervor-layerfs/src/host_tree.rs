use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;

use crate::error::HostTreeError;
use camino::Utf8Path;
use fervor_domain::digest::TreeDigest;
use fervor_domain::files::{EntryKind, FileContent, FileMode, FileOwner, FileSet, LayerEntry};
use fervor_domain::layer::HostTreeSnapshot;
use fervor_domain::path::GuestPath;
use sha2::{Digest, Sha256};

/// Snapshots host directories by walking them in sorted order.
///
/// Ownership and mtimes are dropped and modes normalized (0755 for
/// directories and executables, 0644 otherwise), so the snapshot and its
/// digest depend only on paths, contents, symlink targets and exec bits.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostTreeReader;

impl HostTreeReader {
    pub fn snapshot(
        source: &Utf8Path,
        mount_at: &GuestPath,
    ) -> Result<HostTreeSnapshot, HostTreeError> {
        if !fs::metadata(source).is_ok_and(|m| m.is_dir()) {
            return Err(HostTreeError::NotADirectory(source.to_string()));
        }
        let mut walk = Walk {
            mount_at,
            entries: Vec::new(),
            hasher: TreeHasher::new(),
        };
        walk.entries.push(LayerEntry {
            path: mount_at.clone(),
            kind: EntryKind::Directory {
                mode: FileMode::DIRECTORY,
            },
        });
        walk.dir(source, "")?;
        Ok(HostTreeSnapshot {
            source: source.to_owned(),
            mount_at: mount_at.clone(),
            digest: walk.hasher.finish(),
            files: FileSet {
                owner: FileOwner::HostTree(mount_at.clone()),
                entries: walk.entries,
            },
        })
    }
}

struct Walk<'a> {
    mount_at: &'a GuestPath,
    entries: Vec<LayerEntry>,
    hasher: TreeHasher,
}

impl Walk<'_> {
    /// Visits the children of `dir` (at `relative` below the source root).
    fn dir(&mut self, dir: &Utf8Path, relative: &str) -> Result<(), HostTreeError> {
        let mut children = Vec::new();
        for child in fs::read_dir(dir)
            .map_err(|e| HostTreeError::infrastructure(format!("listing {dir}"), e))?
        {
            let child =
                child.map_err(|e| HostTreeError::infrastructure(format!("listing {dir}"), e))?;
            let name = child.file_name().into_string().map_err(|name| {
                HostTreeError::infrastructure(
                    format!("reading {dir}"),
                    NonUtf8(name.to_string_lossy().into_owned()),
                )
            })?;
            children.push(name);
        }
        children.sort();

        for name in children {
            let host = dir.join(&name);
            let relative = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let path = self.mount_at.join(&relative).map_err(|e| {
                HostTreeError::infrastructure(format!("mapping {host} into the guest"), e)
            })?;
            let meta = fs::symlink_metadata(&host)
                .map_err(|e| HostTreeError::infrastructure(format!("reading {host}"), e))?;
            let file_type = meta.file_type();
            let kind = if file_type.is_dir() {
                self.hasher.entry(&relative, b'd', FileMode::DIRECTORY, &[]);
                EntryKind::Directory {
                    mode: FileMode::DIRECTORY,
                }
            } else if file_type.is_symlink() {
                let target = fs::read_link(&host).map_err(|e| {
                    HostTreeError::infrastructure(format!("reading link {host}"), e)
                })?;
                let target = target.into_os_string().into_string().map_err(|t| {
                    HostTreeError::infrastructure(
                        format!("reading link {host}"),
                        NonUtf8(t.to_string_lossy().into_owned()),
                    )
                })?;
                self.hasher
                    .entry(&relative, b'l', FileMode::new(0o777), target.as_bytes());
                EntryKind::Symlink { target }
            } else if file_type.is_file() {
                let mode = if meta.permissions().mode() & 0o111 != 0 {
                    FileMode::EXECUTABLE
                } else {
                    FileMode::REGULAR
                };
                let content = TreeHasher::file_content(&host)?;
                self.hasher.entry(&relative, b'f', mode, &content);
                EntryKind::File {
                    content: FileContent::HostFile(host.clone()),
                    mode,
                }
            } else {
                return Err(HostTreeError::infrastructure(
                    format!("reading {host}"),
                    UnsupportedFileType,
                ));
            };
            let is_dir = matches!(kind, EntryKind::Directory { .. });
            self.entries.push(LayerEntry { path, kind });
            if is_dir {
                self.dir(&host, &relative)?;
            }
        }
        Ok(())
    }
}

/// Length-prefixed, domain-separated encoding of the sorted tree entries.
struct TreeHasher(Sha256);

impl TreeHasher {
    fn new() -> Self {
        let mut hasher = Self(Sha256::new());
        hasher.bytes(b"fervor/host-tree");
        hasher
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.0.update((bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
    }

    fn entry(&mut self, relative: &str, kind: u8, mode: FileMode, payload: &[u8]) {
        self.bytes(relative.as_bytes());
        self.0.update([kind]);
        self.0.update(mode.bits().to_le_bytes());
        self.bytes(payload);
    }

    fn finish(self) -> TreeDigest {
        TreeDigest::from_bytes(self.0.finalize().into())
    }

    /// sha256 of a host file's bytes, the payload of a file entry.
    fn file_content(path: &Utf8Path) -> Result<[u8; 32], HostTreeError> {
        let mut file = File::open(path)
            .map_err(|e| HostTreeError::infrastructure(format!("opening {path}"), e))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; 64 * 1024];
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(HostTreeError::infrastructure(format!("reading {path}"), e)),
            };
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().into())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not valid UTF-8")]
struct NonUtf8(String);

#[derive(Debug, thiserror::Error)]
#[error("only directories, regular files and symlinks are supported")]
struct UnsupportedFileType;

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use camino::Utf8PathBuf;

    use super::*;

    fn utf8(path: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path.to_owned()).unwrap()
    }

    fn mount() -> GuestPath {
        GuestPath::new("/app").unwrap()
    }

    fn tree() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = utf8(dir.path());
        fs::create_dir_all(root.join("pkg/sub")).unwrap();
        fs::write(root.join("app.py"), "print('hi')\n").unwrap();
        fs::write(root.join("pkg/sub/data.txt"), "data").unwrap();
        fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(root.join("app.py"), fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink("app.py", root.join("main.py")).unwrap();
        (dir, root)
    }

    fn digest(root: &Utf8Path) -> TreeDigest {
        HostTreeReader::snapshot(root, &mount()).unwrap().digest
    }

    #[test]
    fn snapshot_maps_paths_and_normalizes_modes() {
        let (_dir, root) = tree();
        let snapshot = HostTreeReader::snapshot(&root, &mount()).unwrap();
        assert_eq!(snapshot.files.owner, FileOwner::HostTree(mount()));
        let entries: Vec<(&str, EntryKind)> = snapshot
            .files
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.kind.clone()))
            .collect();
        let dir = EntryKind::Directory {
            mode: FileMode::DIRECTORY,
        };
        let file = |rel: &str, mode| EntryKind::File {
            content: FileContent::HostFile(root.join(rel)),
            mode,
        };
        assert_eq!(
            entries,
            vec![
                ("/app", dir.clone()),
                ("/app/app.py", file("app.py", FileMode::REGULAR)),
                (
                    "/app/main.py",
                    EntryKind::Symlink {
                        target: "app.py".into()
                    }
                ),
                ("/app/pkg", dir.clone()),
                ("/app/pkg/sub", dir),
                (
                    "/app/pkg/sub/data.txt",
                    file("pkg/sub/data.txt", FileMode::REGULAR)
                ),
                ("/app/run.sh", file("run.sh", FileMode::EXECUTABLE)),
            ]
        );
    }

    #[test]
    fn digest_ignores_mtimes_but_tracks_content_and_exec_bit() {
        let (_dir, root) = tree();
        let base = digest(&root);

        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        File::options()
            .write(true)
            .open(root.join("app.py"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        fs::set_permissions(root.join("app.py"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            digest(&root),
            base,
            "mtime and non-exec mode bits are not part of the digest"
        );

        fs::write(root.join("pkg/sub/data.txt"), "DATA").unwrap();
        let changed = digest(&root);
        assert_ne!(changed, base);

        fs::set_permissions(
            root.join("pkg/sub/data.txt"),
            fs::Permissions::from_mode(0o744),
        )
        .unwrap();
        assert_ne!(digest(&root), changed);
    }

    #[test]
    fn missing_or_file_source_is_not_a_directory() {
        let (_dir, root) = tree();
        for source in [root.join("nope"), root.join("app.py")] {
            assert!(matches!(
                HostTreeReader::snapshot(&source, &mount()),
                Err(HostTreeError::NotADirectory(_))
            ));
        }
    }
}
