use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, Cursor, Write};

use crate::error::BuildError;
use backhand::compression::Compressor;
use backhand::{FilesystemCompressor, FilesystemWriter, NodeHeader};
use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::files::{EntryKind, FileContent, FileMode, FileSet};
use fervor_domain::layer::{IndexEntry, LayerBlob, LayerIndex};
use fervor_domain::path::GuestPath;
use fervor_domain::size::ByteSize;
use fervor_store::BlobFile;

/// SquashFS data block size.
const BLOCK_SIZE: u32 = 128 * 1024;

/// Writes file sets into a single zstd-compressed SquashFS image.
///
/// Every node is owned by `root:root` with mtime 0 and the superblock time is
/// 0, so the image bytes depend only on the logical content.
#[derive(Debug, Clone)]
pub struct SquashfsLayerBuilder {
    tmp_dir: Utf8PathBuf,
}

impl SquashfsLayerBuilder {
    /// Images are written as temporary files in `tmp_dir`; pass the layer
    /// store's tmp dir so the store can rename them into place.
    pub fn new(tmp_dir: impl Into<Utf8PathBuf>) -> Self {
        Self {
            tmp_dir: tmp_dir.into(),
        }
    }

    /// Later sets win if they contain the same path (callers reject conflicts
    /// before building unless the conflict policy allows them).
    pub fn build(&self, contents: &[FileSet]) -> Result<LayerBlob, BuildError> {
        let tree = MergedTree::of(contents);
        let mut image = SquashfsWriter::new()?;
        for (path, kind) in &tree.entries {
            image.push(path, kind)?;
        }
        let path = image.write_in(&self.tmp_dir)?;

        let size = std::fs::metadata(&path)
            .map_err(|e| BuildError::new(format!("reading size of {path}"), e))?
            .len();
        let digest = BlobFile::digest_of(&path)
            .map_err(|e| BuildError::new(format!("hashing {path}"), e))?;
        Ok(LayerBlob {
            path,
            digest,
            size: ByteSize::bytes(size),
            index: tree.index,
        })
    }
}

/// Directory mode of parents no set lists explicitly.
static IMPLICIT_DIRECTORY: EntryKind = EntryKind::Directory {
    mode: FileMode::DIRECTORY,
};

/// The file sets of one layer laid over each other.
struct MergedTree<'c> {
    /// Final entry per path (later sets win) plus implicit parent
    /// directories. Sorted by path, so every parent precedes its children.
    entries: BTreeMap<GuestPath, &'c EntryKind>,
    /// One entry per (path, owner) for every non-directory entry of every
    /// set, including entries overridden by later sets, so conflicts stay
    /// visible. Sorted by path; owners of one path keep set order.
    index: LayerIndex,
}

impl<'c> MergedTree<'c> {
    fn of(contents: &'c [FileSet]) -> Self {
        Self {
            entries: Self::merge(contents),
            index: Self::index(contents),
        }
    }

    fn merge(contents: &'c [FileSet]) -> BTreeMap<GuestPath, &'c EntryKind> {
        let mut entries = BTreeMap::new();
        for set in contents {
            for entry in &set.entries {
                entries.insert(entry.path.clone(), &entry.kind);
            }
        }
        let mut parents = Vec::new();
        for path in entries.keys() {
            let mut current = path.parent();
            while let Some(parent) = current {
                if parent.is_root() || entries.contains_key(&parent) {
                    break;
                }
                current = parent.parent();
                parents.push(parent);
            }
        }
        for parent in parents {
            entries.entry(parent).or_insert(&IMPLICIT_DIRECTORY);
        }
        entries
    }

    fn index(contents: &[FileSet]) -> LayerIndex {
        let mut entries: Vec<IndexEntry> = Vec::new();
        for set in contents {
            for entry in &set.entries {
                if !matches!(entry.kind, EntryKind::Directory { .. }) {
                    entries.push(IndexEntry {
                        path: entry.path.clone(),
                        owner: set.owner.clone(),
                    });
                }
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        let mut seen = BTreeSet::new();
        entries.retain(|e| seen.insert((e.path.clone(), e.owner.clone())));
        LayerIndex { entries }
    }
}

/// A SquashFS image being assembled: zstd, 128 KiB blocks, 4 KiB padding,
/// every node `root:root` with mtime 0.
struct SquashfsWriter<'c> {
    fs: FilesystemWriter<'static, 'static, 'c>,
}

impl<'c> SquashfsWriter<'c> {
    fn new() -> Result<Self, BuildError> {
        let mut fs = FilesystemWriter::default();
        fs.set_compressor(
            FilesystemCompressor::new(Compressor::Zstd, None)
                .map_err(|e| BuildError::new("configuring zstd compression", e))?,
        );
        fs.set_block_size(BLOCK_SIZE);
        fs.set_time(0);
        fs.set_root_mode(FileMode::DIRECTORY.bits());
        fs.set_root_uid(0);
        fs.set_root_gid(0);
        // Whole 4 KiB blocks, so the image can be attached as a block device.
        fs.set_kib_padding(4);
        Ok(Self { fs })
    }

    /// Adds one node; parents must already have been pushed.
    fn push(&mut self, path: &GuestPath, kind: &'c EntryKind) -> Result<(), BuildError> {
        if path.is_root() {
            return match kind {
                EntryKind::Directory { mode } => {
                    self.fs.set_root_mode(mode.bits());
                    Ok(())
                }
                _ => Err(BuildError::new("adding /", NonDirectoryRoot)),
            };
        }
        let target = path.as_str();
        let result = match kind {
            EntryKind::Directory { mode } => self.fs.push_dir(target, Self::header(*mode)),
            EntryKind::Symlink { target: link } => {
                self.fs
                    .push_symlink(link.as_str(), target, Self::header(FileMode::new(0o777)))
            }
            EntryKind::File {
                content: FileContent::Bytes(bytes),
                mode,
            } => self
                .fs
                .push_file(Cursor::new(bytes.as_slice()), target, Self::header(*mode)),
            EntryKind::File {
                content: FileContent::HostFile(source),
                mode,
            } => {
                // Fail early with a clear path instead of a bare I/O error at write time.
                File::open(source)
                    .map_err(|e| BuildError::new(format!("opening {source} for {path}"), e))?;
                self.fs
                    .push_file_from_path(source.as_std_path(), target, Self::header(*mode))
            }
        };
        result.map_err(|e| BuildError::new(format!("adding {path}"), e))
    }

    /// Root-owned, mtime 0.
    fn header(mode: FileMode) -> NodeHeader {
        NodeHeader::new(mode.bits(), 0, 0, 0)
    }

    /// Writes the image as a new, synced file in `tmp_dir` and returns its path.
    fn write_in(mut self, tmp_dir: &Utf8Path) -> Result<Utf8PathBuf, BuildError> {
        std::fs::create_dir_all(tmp_dir)
            .map_err(|e| BuildError::new(format!("creating {tmp_dir}"), e))?;
        let mut file = tempfile::NamedTempFile::new_in(tmp_dir)
            .map_err(|e| BuildError::new(format!("creating a temporary layer in {tmp_dir}"), e))?;
        {
            let mut out = BufWriter::new(file.as_file_mut());
            self.fs
                .write(&mut out)
                .map_err(|e| BuildError::new("writing squashfs image", e))?;
            out.flush()
                .map_err(|e| BuildError::new("writing squashfs image", e))?;
        }
        file.as_file()
            .sync_all()
            .map_err(|e| BuildError::new("syncing squashfs image", e))?;
        let (_, path) = file
            .keep()
            .map_err(|e| BuildError::new("keeping squashfs image", e.error))?;
        Utf8PathBuf::from_path_buf(path)
            .map_err(|p| BuildError::new("temporary layer path", NonUtf8(p.display().to_string())))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the root of a layer must be a directory")]
struct NonDirectoryRoot;

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not valid UTF-8")]
struct NonUtf8(String);

#[cfg(test)]
mod tests {
    use backhand::{FilesystemReader, InnerNode, SquashfsFileReader};
    use fervor_domain::files::{FileOwner, LayerEntry};

    use super::*;

    fn path(p: &str) -> GuestPath {
        GuestPath::new(p).unwrap()
    }

    fn file(p: &str, bytes: &[u8], mode: u32) -> LayerEntry {
        LayerEntry {
            path: path(p),
            kind: EntryKind::File {
                content: FileContent::Bytes(bytes.to_vec()),
                mode: FileMode::new(mode),
            },
        }
    }

    fn set(owner: &str, entries: Vec<LayerEntry>) -> FileSet {
        FileSet {
            owner: FileOwner::Package(owner.into()),
            entries,
        }
    }

    fn sample() -> Vec<FileSet> {
        vec![
            set(
                "a",
                vec![
                    file("/opt/env/bin/tool", b"#!/bin/sh\necho hi\n", 0o755),
                    LayerEntry {
                        path: path("/opt/env/lib"),
                        kind: EntryKind::Directory {
                            mode: FileMode::new(0o700),
                        },
                    },
                    file("/opt/env/lib/libx.so", &[7; 300_000], 0o644),
                ],
            ),
            set(
                "b",
                vec![
                    LayerEntry {
                        path: path("/opt/env/bin/link"),
                        kind: EntryKind::Symlink {
                            target: "tool".into(),
                        },
                    },
                    file("/etc/conf", b"x=1\n", 0o600),
                ],
            ),
        ]
    }

    fn reversed(sets: &[FileSet]) -> Vec<FileSet> {
        sets.iter()
            .rev()
            .map(|s| FileSet {
                owner: s.owner.clone(),
                entries: s.entries.iter().rev().cloned().collect(),
            })
            .collect()
    }

    fn read_file(reader: &FilesystemReader<'_>, file: &SquashfsFileReader) -> Vec<u8> {
        let mut out = Vec::new();
        std::io::copy(&mut reader.file(file).reader(), &mut out).unwrap();
        out
    }

    #[test]
    fn identical_content_gives_identical_bytes_regardless_of_order() {
        let tmp = tempfile::tempdir().unwrap();
        let builder =
            SquashfsLayerBuilder::new(Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
        let first = builder.build(&sample()).unwrap();
        let second = builder.build(&sample()).unwrap();
        let shuffled = builder.build(&reversed(&sample())).unwrap();

        assert_eq!(first.digest, second.digest);
        assert_eq!(first.digest, shuffled.digest);
        assert_eq!(
            std::fs::read(&first.path).unwrap(),
            std::fs::read(&shuffled.path).unwrap()
        );
        assert_ne!(first.path, second.path);
        let indexed: Vec<&str> = first
            .index
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert_eq!(
            indexed,
            [
                "/etc/conf",
                "/opt/env/bin/link",
                "/opt/env/bin/tool",
                "/opt/env/lib/libx.so"
            ]
        );
    }

    #[test]
    fn image_has_root_owned_nodes_with_requested_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let builder =
            SquashfsLayerBuilder::new(Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
        let blob = builder.build(&sample()).unwrap();
        assert_eq!(
            blob.size.as_u64(),
            std::fs::metadata(&blob.path).unwrap().len()
        );
        assert_eq!(blob.size.as_u64() % 4096, 0);

        let reader =
            FilesystemReader::from_reader(std::io::BufReader::new(File::open(&blob.path).unwrap()))
                .unwrap();
        let mut seen = BTreeMap::new();
        for node in reader.files() {
            assert_eq!(
                (node.header.uid, node.header.gid, node.header.mtime),
                (0, 0, 0),
                "{:?}",
                node.fullpath
            );
            let kind = match &node.inner {
                InnerNode::Dir(_) => "dir".to_owned(),
                InnerNode::Symlink(link) => format!("-> {}", link.link.display()),
                InnerNode::File(file) => format!("{} bytes", read_file(&reader, file).len()),
                _ => "other".to_owned(),
            };
            seen.insert(
                node.fullpath.display().to_string(),
                (node.header.permissions, kind),
            );
        }
        let expect = |p: &str, mode: u16, kind: &str| (p.to_owned(), (mode, kind.to_owned()));
        assert_eq!(
            seen,
            BTreeMap::from([
                expect("/", 0o755, "dir"),
                expect("/etc", 0o755, "dir"),
                expect("/etc/conf", 0o600, "4 bytes"),
                expect("/opt", 0o755, "dir"),
                expect("/opt/env", 0o755, "dir"),
                expect("/opt/env/bin", 0o755, "dir"),
                expect("/opt/env/bin/link", 0o777, "-> tool"),
                expect("/opt/env/bin/tool", 0o755, "18 bytes"),
                expect("/opt/env/lib", 0o700, "dir"),
                expect("/opt/env/lib/libx.so", 0o644, "300000 bytes"),
            ])
        );
    }

    #[test]
    fn later_set_wins_on_duplicate_path() {
        let tmp = tempfile::tempdir().unwrap();
        let builder =
            SquashfsLayerBuilder::new(Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
        let blob = builder
            .build(&[
                set("old", vec![file("/a", b"old", 0o644)]),
                set("new", vec![file("/a", b"new", 0o755)]),
            ])
            .unwrap();
        assert_eq!(
            blob.index.entries,
            vec![
                IndexEntry {
                    path: path("/a"),
                    owner: FileOwner::Package("old".into())
                },
                IndexEntry {
                    path: path("/a"),
                    owner: FileOwner::Package("new".into())
                },
            ]
        );

        let reader =
            FilesystemReader::from_reader(std::io::BufReader::new(File::open(&blob.path).unwrap()))
                .unwrap();
        let node = reader
            .files()
            .find(|n| n.fullpath.as_os_str() == "/a")
            .unwrap();
        let InnerNode::File(f) = &node.inner else {
            panic!("not a file")
        };
        assert_eq!(
            (node.header.permissions, read_file(&reader, f)),
            (0o755, b"new".to_vec())
        );
    }

    #[test]
    fn missing_host_file_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let builder =
            SquashfsLayerBuilder::new(Utf8PathBuf::from_path_buf(tmp.path().to_owned()).unwrap());
        let entry = LayerEntry {
            path: path("/x"),
            kind: EntryKind::File {
                content: FileContent::HostFile(
                    Utf8PathBuf::from_path_buf(tmp.path().join("missing")).unwrap(),
                ),
                mode: FileMode::REGULAR,
            },
        };
        assert!(builder.build(&[set("a", vec![entry])]).is_err());
    }
}
