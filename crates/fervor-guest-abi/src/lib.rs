//! The contract between the fervor host and `fervor-init` running as PID 1 in
//! the guest. Both sides compile this crate, so it stays dependency-light and
//! free of host concepts (layer keys, rattler, stores).

pub mod backoff;
pub mod config;
pub mod control;
pub mod pack;

pub use backoff::{Backoff, Retry};
pub use config::{GuestConfig, JournalConfig, NetworkConfig, RunConfig, ServiceConfig};
pub use control::{ExitReport, ForwardPreamble, GuestToHost, HostToGuest};
pub use pack::{PackConfig, PackEntry, PackError, PackHeader};

/// Block device of the read-only boot layer (kernel `root=`).
pub const BOOT_DEVICE: &str = "/dev/vda";
/// Block device holding the packed layers of the image.
pub const PACK_DEVICE: &str = "/dev/vdb";
/// Block device holding the run pack: per-run config and layers.
pub const RUN_DEVICE: &str = "/dev/vdc";

/// vsock CID of the host side (fixed by virtio-vsock).
pub const HOST_CID: u32 = 2;
/// vsock CID assigned to the guest.
pub const GUEST_CID: u32 = 3;
/// Guest → host control connection (init connects to the host on this port).
pub const CONTROL_PORT: u32 = 1024;
/// Host → guest forwarding connections (init listens on this port).
pub const FORWARD_PORT: u32 = 1025;
