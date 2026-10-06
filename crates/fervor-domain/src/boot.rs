//! The boot layer: the read-only root the kernel mounts. It carries
//! `fervor-init`, the guest glibc (taken from the conda-forge sysroot) and a
//! minimal `/etc`; everything else arrives through the layer stack.

use crate::digest::{ArtifactDigest, LayerKey};
use crate::environment::ResolvedPackage;
use crate::files::{ArchiveEntry, EntryKind, FileContent, FileMode, FileOwner, FileSet, LayerEntry};
use crate::layer::{LayerKeyInput, LayoutVersion};
use crate::path::GuestPath;
use crate::platform::GuestPlatform;

/// Path of the init binary inside the boot layer (kernel `init=`).
pub const INIT_PATH: &str = "/sbin/init";
/// Where glibc's shared objects are installed.
pub const LIB_DIR: &str = "/lib64";
/// Hostname of every guest.
pub const HOSTNAME: &str = "fervor";
/// Mount points `fervor-init` uses while assembling the stack.
pub const STAGING_DIR: &str = "/fervor";

/// The init binary that runs as PID 1.
#[derive(Debug, Clone)]
pub struct InitBinary {
    pub bytes: Vec<u8>,
    pub digest: ArtifactDigest,
}

impl InitBinary {
    pub fn new(bytes: Vec<u8>) -> Self {
        let digest = ArtifactDigest::of_bytes(&bytes);
        Self { bytes, digest }
    }
}

/// Everything the boot layer is made from.
#[derive(Debug, Clone)]
pub struct BootLayerSpec {
    pub platform: GuestPlatform,
    pub sysroot: ResolvedPackage,
    pub init: InitBinary,
}

impl BootLayerSpec {
    pub fn key(&self, layout: LayoutVersion) -> LayerKey {
        LayerKey::derive(
            layout,
            LayerKeyInput::Boot {
                platform: self.platform,
                sysroot: self.sysroot.sha256(),
                init: self.init.digest,
            },
        )
    }

    pub fn label(&self) -> String {
        format!("boot (fervor-init, {})", self.sysroot.display_name())
    }

    /// Assembles the boot layer from the sysroot archive's entries.
    ///
    /// Only glibc runtime objects (`lib*.so.N…` and the program interpreter)
    /// are taken; headers, static archives and linker scripts are dropped.
    pub fn files(&self, sysroot_entries: Vec<ArchiveEntry>) -> FileSet {
        let lib_prefix = format!("{}/lib64/", self.platform.sysroot_root());
        let mut entries = Self::skeleton();

        for entry in sysroot_entries {
            let Some(object) = entry.path.strip_prefix(&lib_prefix).and_then(RuntimeObject::from_file_name) else {
                continue;
            };
            entries.push(LayerEntry { path: object.guest_path(), kind: entry.kind });
        }

        entries.push(LayerEntry {
            path: GuestPath::new(INIT_PATH).expect("valid path"),
            kind: EntryKind::File {
                content: FileContent::Bytes(self.init.bytes.clone()),
                mode: FileMode::EXECUTABLE,
            },
        });
        FileSet { owner: FileOwner::Boot, entries }
    }

    /// Mount points, the `/lib` alias and a minimal `/etc`.
    fn skeleton() -> Vec<LayerEntry> {
        let dir = |path: &str, mode: u32| LayerEntry {
            path: GuestPath::new(path).expect("valid path"),
            kind: EntryKind::Directory { mode: FileMode::new(mode) },
        };
        let file = |path: &str, content: &str| LayerEntry {
            path: GuestPath::new(path).expect("valid path"),
            kind: EntryKind::File {
                content: FileContent::Bytes(content.as_bytes().to_vec()),
                mode: FileMode::REGULAR,
            },
        };
        vec![
            dir("/dev", 0o755),
            dir("/proc", 0o555),
            dir("/sys", 0o555),
            dir("/run", 0o755),
            dir("/tmp", 0o1777),
            dir("/root", 0o700),
            dir("/opt", 0o755),
            dir(STAGING_DIR, 0o700),
            LayerEntry {
                path: GuestPath::new("/lib").expect("valid path"),
                kind: EntryKind::Symlink { target: "lib64".to_owned() },
            },
            file("/etc/hostname", &format!("{HOSTNAME}\n")),
            file("/etc/hosts", &format!("127.0.0.1 localhost {HOSTNAME}\n::1 localhost\n")),
            file(
                "/etc/passwd",
                "root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/nonexistent:/bin/false\n",
            ),
            file("/etc/group", "root:x:0:\nnogroup:x:65534:\n"),
            file("/etc/nsswitch.conf", "passwd: files\ngroup: files\nhosts: files dns\n"),
        ]
    }
}

/// A glibc shared object the guest needs at run time: `libc.so.6`,
/// `libnss_files.so.2`, `ld-linux-aarch64.so.1`, … — but not `libc.so` (a
/// linker script), static archives or debug files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeObject<'a> {
    file_name: &'a str,
}

impl<'a> RuntimeObject<'a> {
    /// `None` unless `file_name` is a bare `*.so.<digits>[.<digits>…]` name.
    pub fn from_file_name(file_name: &'a str) -> Option<Self> {
        if file_name.contains('/') {
            return None;
        }
        let (_, version) = file_name.split_once(".so.")?;
        let numeric = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
        version.split('.').all(numeric).then_some(Self { file_name })
    }

    /// Where the object is installed in the boot layer.
    pub fn guest_path(self) -> GuestPath {
        GuestPath::new(format!("{LIB_DIR}/{}", self.file_name)).expect("valid library path")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_objects_exclude_linker_scripts_and_archives() {
        for keep in ["libc.so.6", "ld-linux-aarch64.so.1", "libnss_files.so.2", "libm.so.6"] {
            assert!(RuntimeObject::from_file_name(keep).is_some(), "{keep}");
        }
        for drop in ["libc.so", "libc.a", "libc_nonshared.a", "libpthread.so.0.debug", "libc.so.", "gconv/libX.so.1"] {
            assert!(RuntimeObject::from_file_name(drop).is_none(), "{drop}");
        }
    }
}
