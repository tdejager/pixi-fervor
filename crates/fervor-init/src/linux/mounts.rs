//! Pseudo filesystems, loop-mounted layers, the overlay root and the switch
//! into it.

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

use fervor_guest_abi::{PACK_DEVICE, PackEntry, PackHeader};
use rustix::io::Errno;
use rustix::mount::{
    FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MoveMountFlags, UnmountFlags,
    fsconfig_create, fsconfig_set_string, fsmount, fsopen, mount, mount_bind, mount_move,
    move_mount, unmount,
};

use super::{Context, Error};
use crate::plan::{LoopRange, OverlayLowers};

/// Staging area: an empty directory in the boot layer, covered by a tmpfs so
/// that mount points can be created below it.
const STAGING: &str = "/fervor";
const LAYERS: &str = "/fervor/l";
const BOOT_ROOT: &str = "/fervor/boot";
const SCRATCH: &str = "/fervor/scratch";
const NEW_ROOT: &str = "/fervor/root";

/// Mount points carried over from the boot root into the overlay root.
const CARRIED_MOUNTS: [&str; 4] = ["/proc", "/sys", "/dev", "/run"];

const PSEUDO: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::NOEXEC);

/// A directory something gets mounted on.
struct MountPoint<'a>(&'a Path);

impl<'a> MountPoint<'a> {
    fn new(path: &'a (impl AsRef<Path> + ?Sized)) -> Self {
        Self(path.as_ref())
    }

    /// Creates the directory unless it already exists.
    fn create(self, mode: u32) -> Result<Self, Error> {
        match std::fs::DirBuilder::new().mode(mode).create(self.0) {
            Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
                Err(err).context(|| format!("creating {}", self.0.display()))
            }
            _ => Ok(self),
        }
    }

    /// Mounts a source-less filesystem (`fstype` doubles as the source).
    fn mount_fs(&self, fstype: &str, flags: MountFlags, data: Option<&CStr>) -> Result<(), Error> {
        mount(fstype, self.0, fstype, flags, data)
            .context(|| format!("mounting {fstype} on {}", self.0.display()))
    }
}

/// The pseudo filesystems init and the entrypoint need before anything else.
pub struct EarlyMounts;

impl EarlyMounts {
    pub fn mount() -> Result<(), Error> {
        MountPoint::new("/proc").mount_fs("proc", PSEUDO, None)?;
        MountPoint::new("/sys").mount_fs("sysfs", PSEUDO, None)?;
        // The kernel mounts devtmpfs on /dev itself when built with
        // CONFIG_DEVTMPFS_MOUNT; mounting it again there reports EBUSY.
        match mount(
            "devtmpfs",
            "/dev",
            "devtmpfs",
            MountFlags::NOSUID,
            c"mode=0755",
        ) {
            Ok(()) | Err(Errno::BUSY) => {}
            Err(err) => return Err(err).context(|| "mounting devtmpfs on /dev".to_owned()),
        }
        MountPoint::new("/dev/pts").create(0o755)?;
        mount(
            "devpts",
            "/dev/pts",
            "devpts",
            MountFlags::NOSUID.union(MountFlags::NOEXEC),
            c"mode=0620,ptmxmode=0666",
        )
        .context(|| "mounting devpts on /dev/pts".to_owned())?;
        MountPoint::new("/dev/shm").create(0o1777)?.mount_fs(
            "tmpfs",
            MountFlags::NOSUID.union(MountFlags::NODEV),
            Some(c"mode=1777"),
        )?;
        for (link, target) in [
            ("/dev/fd", "/proc/self/fd"),
            ("/dev/stdin", "/proc/self/fd/0"),
            ("/dev/stdout", "/proc/self/fd/1"),
            ("/dev/stderr", "/proc/self/fd/2"),
            ("/dev/ptmx", "pts/ptmx"),
        ] {
            match symlink(target, link) {
                Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
                    return Err(err).context(|| format!("linking {link} to {target}"));
                }
                _ => {}
            }
        }
        MountPoint::new("/run").mount_fs(
            "tmpfs",
            MountFlags::NOSUID.union(MountFlags::NODEV),
            Some(c"mode=0755"),
        )
    }
}

/// Builds the overlay root from the layers of an open pack device.
pub struct RootAssembler<'a> {
    pub pack: &'a File,
    pub header: &'a PackHeader,
}

impl RootAssembler<'_> {
    /// Loop-mounts every pack layer and stacks them with the boot root under
    /// a tmpfs-backed writable overlay at [`NEW_ROOT`].
    pub fn assemble(&self) -> Result<NewRoot, Error> {
        MountPoint::new(STAGING).mount_fs(
            "tmpfs",
            MountFlags::NOSUID.union(MountFlags::NODEV),
            Some(c"mode=0700"),
        )?;
        for dir in [LAYERS, BOOT_ROOT, SCRATCH, NEW_ROOT] {
            MountPoint::new(dir).create(0o755)?;
        }
        // Non-recursive: only the boot squashfs, without /proc, /fervor, ….
        mount_bind("/", BOOT_ROOT).context(|| format!("binding the boot root to {BOOT_ROOT}"))?;

        let loop_control = LoopControl::open()?;
        let layer_mounts = self
            .header
            .layers()
            .iter()
            .enumerate()
            .map(|(index, entry)| LayerMount { index, entry }.mount(&loop_control, self.pack))
            .collect::<Result<Vec<_>, _>>()?;

        let scratch = Scratch::mount(self.header.config().scratch_size_mib)?;
        let overlay = Overlay {
            lowers: OverlayLowers::new(&layer_mounts, Path::new(BOOT_ROOT)),
            scratch,
        };
        let root = NewRoot {
            path: PathBuf::from(NEW_ROOT),
        };
        overlay.mount(&root.path)?;
        Ok(root)
    }
}

/// `/dev/loop-control`, which hands out free loop devices.
struct LoopControl(File);

impl LoopControl {
    fn open() -> Result<Self, Error> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/loop-control")
            .map(Self)
            .context(|| "opening /dev/loop-control".to_owned())
    }

    fn free_device_number(&self) -> Result<libc::c_int, Error> {
        // SAFETY: LOOP_CTL_GET_FREE takes no argument and returns a device number.
        let number = unsafe { libc::ioctl(self.0.as_raw_fd(), LOOP_CTL_GET_FREE) };
        if number < 0 {
            return Err(io::Error::last_os_error())
                .context(|| "allocating a loop device".to_owned());
        }
        Ok(number)
    }
}

/// A read-only, auto-clearing loop device exposing one layer of the pack.
///
/// AUTOCLEAR detaches the device on its last close, so the value must stay
/// alive until a mount holds the device; afterwards the device lives exactly
/// as long as the mount.
struct LoopDevice {
    path: PathBuf,
    _open: File,
}

impl LoopDevice {
    fn attach(control: &LoopControl, pack: &File, entry: &PackEntry) -> Result<Self, Error> {
        let path = PathBuf::from(format!("/dev/loop{}", control.free_device_number()?));
        // Read-only: the kernel refuses to mount a block device that is open
        // for writing elsewhere.
        let open = File::open(&path).context(|| format!("opening {}", path.display()))?;

        let range = LoopRange::of(entry);
        let config = LoopConfig {
            fd: pack.as_raw_fd() as u32,
            block_size: 0,
            info: LoopInfo64 {
                lo_device: 0,
                lo_inode: 0,
                lo_rdevice: 0,
                lo_offset: range.offset,
                lo_sizelimit: range.size_limit,
                lo_number: 0,
                lo_encrypt_type: 0,
                lo_encrypt_key_size: 0,
                lo_flags: LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR,
                lo_file_name: [0; 64],
                lo_crypt_name: [0; 64],
                lo_encrypt_key: [0; 32],
                lo_init: [0; 2],
            },
            reserved: [0; 8],
        };
        // SAFETY: `config` is a correctly laid out `struct loop_config` that
        // outlives the call; the kernel only reads it.
        if unsafe { libc::ioctl(open.as_raw_fd(), LOOP_CONFIGURE, &config) } < 0 {
            return Err(io::Error::last_os_error()).context(|| {
                format!(
                    "configuring {} on {PACK_DEVICE} at offset {} ({} bytes)",
                    path.display(),
                    entry.offset,
                    entry.len
                )
            });
        }
        Ok(Self { path, _open: open })
    }
}

/// One pack layer, mounted read-only at `/fervor/l/<index>`.
struct LayerMount<'a> {
    index: usize,
    entry: &'a PackEntry,
}

impl LayerMount<'_> {
    /// Mounts the layer's SquashFS and returns its mount point.
    fn mount(&self, loop_control: &LoopControl, pack: &File) -> Result<PathBuf, Error> {
        let target = PathBuf::from(format!("{LAYERS}/{}", self.index));
        MountPoint::new(&target).create(0o755)?;
        self.mount_at(loop_control, pack, &target).map_err(|err| {
            Error::msg(format!(
                "mounting layer {} ({}): {err}",
                self.index,
                self.short_digest()
            ))
        })?;
        Ok(target)
    }

    fn mount_at(
        &self,
        loop_control: &LoopControl,
        pack: &File,
        target: &Path,
    ) -> Result<(), Error> {
        if self.entry.len == 0 {
            return Err(Error::msg("the layer is empty"));
        }
        let device = LoopDevice::attach(loop_control, pack, self.entry)?;
        mount(&device.path, target, "squashfs", MountFlags::RDONLY, None).context(|| {
            format!(
                "mounting {} (squashfs) on {}",
                device.path.display(),
                target.display()
            )
        })
    }

    fn short_digest(&self) -> String {
        self.entry.digest[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// The size-limited tmpfs holding the overlay's upper and work directories.
struct Scratch {
    upper: PathBuf,
    work: PathBuf,
}

impl Scratch {
    fn mount(size_mib: u32) -> Result<Self, Error> {
        mount(
            "tmpfs",
            SCRATCH,
            "tmpfs",
            MountFlags::NOSUID.union(MountFlags::NODEV),
            Some(&*Self::options(size_mib)),
        )
        .context(|| format!("mounting the {size_mib} MiB scratch tmpfs on {SCRATCH}"))?;
        let scratch = Self {
            upper: PathBuf::from(format!("{SCRATCH}/upper")),
            work: PathBuf::from(format!("{SCRATCH}/work")),
        };
        MountPoint::new(&scratch.upper).create(0o755)?;
        MountPoint::new(&scratch.work).create(0o755)?;
        Ok(scratch)
    }

    fn options(size_mib: u32) -> CString {
        CString::new(format!("mode=0755,size={size_mib}m"))
            .expect("mount options never contain NUL")
    }
}

/// The writable overlay of the layer stack.
struct Overlay {
    lowers: OverlayLowers,
    scratch: Scratch,
}

impl Overlay {
    /// Mounts the overlay at `target` with the new mount API: `lowerdir+`
    /// takes one layer per call, so the stack depth is not bounded by the
    /// 4 KiB option page.
    fn mount(&self, target: &Path) -> Result<(), Error> {
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)
            .context(|| "opening an overlay context".to_owned())?;
        let configure = || -> Result<OwnedFd, Errno> {
            for lower in self.lowers.as_slice() {
                fsconfig_set_string(&fs, "lowerdir+", lower)?;
            }
            fsconfig_set_string(&fs, "upperdir", &self.scratch.upper)?;
            fsconfig_set_string(&fs, "workdir", &self.scratch.work)?;
            fsconfig_create(&fs)?;
            fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())
        };
        let mnt = configure().map_err(|errno| {
            let log = Self::drain_fs_log(&fs);
            let detail = if log.is_empty() {
                String::new()
            } else {
                format!(" [{log}]")
            };
            Error::msg(format!(
                "creating the overlay of {} layers: {errno}{detail}",
                self.lowers.as_slice().len()
            ))
        })?;
        move_mount(
            &mnt,
            "",
            rustix::fs::CWD,
            target,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
        .context(|| format!("attaching the overlay at {}", target.display()))
    }

    /// Messages the kernel queued on a filesystem context (one per read).
    fn drain_fs_log(fs: impl AsFd) -> String {
        let mut messages = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n @ 1..) = rustix::io::read(&fs, &mut buf) {
            messages.push(String::from_utf8_lossy(&buf[..n]).trim_end().to_owned());
        }
        messages.join("; ")
    }
}

/// The assembled overlay root, not yet switched into.
pub struct NewRoot {
    path: PathBuf,
}

impl NewRoot {
    /// Moves the pseudo filesystems into the new root, gives it a fresh /tmp,
    /// pivots into it and lazily detaches the boot root (the overlay keeps its
    /// layers).
    pub fn switch(self) -> Result<(), Error> {
        let root = &self.path;
        for dir in CARRIED_MOUNTS {
            let target = root.join(&dir[1..]);
            MountPoint::new(&target).create(0o755)?;
            mount_move(dir, &target).context(|| format!("moving {dir} to {}", target.display()))?;
        }
        MountPoint::new(&root.join("tmp"))
            .create(0o1777)?
            .mount_fs(
                "tmpfs",
                MountFlags::NOSUID.union(MountFlags::NODEV),
                Some(c"mode=1777"),
            )?;

        let pivot = || "pivoting into the overlay root".to_owned();
        rustix::process::chdir(root).context(pivot)?;
        // pivot_root(".", ".") stacks the old root on top of the new one, so a
        // lazy unmount of "." drops it without needing a put_old directory.
        rustix::process::pivot_root(".", ".").context(pivot)?;
        unmount(".", UnmountFlags::DETACH).context(|| "detaching the boot root".to_owned())?;
        rustix::process::chdir("/").context(pivot)
    }
}

const LOOP_CTL_GET_FREE: libc::Ioctl = 0x4C82;
const LOOP_CONFIGURE: libc::Ioctl = 0x4C0A;
const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;

/// `struct loop_info64` from `<linux/loop.h>`.
#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

/// `struct loop_config` from `<linux/loop.h>`.
#[repr(C)]
struct LoopConfig {
    fd: u32,
    block_size: u32,
    info: LoopInfo64,
    reserved: [u64; 8],
}

const _: () = assert!(std::mem::size_of::<LoopInfo64>() == 232);
const _: () = assert!(std::mem::size_of::<LoopConfig>() == 304);
