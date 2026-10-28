//! Boots fervor images in Firecracker on a Linux host with KVM.
//!
//! - [`pinned_artifacts`]: selects the Firecracker and guest kernel releases
//! - [`ArtifactCache`]: downloads and verifies the pinned Firecracker binary and guest kernel
//! - [`Packer`]: concatenates layers behind a [`PackHeader`](fervor_guest_abi::PackHeader): the image pack and the per-run pack
//! - [`FirecrackerConfig`]: the `--config-file` handed to Firecracker
//! - [`LocalRunHost`]: imports an image's and a run's layers, packs them, sets up the network and boots the VM
//! - [`RunnerOutcome`]: the outcome line `fervor-runner` prints for a remote caller

mod artifacts;
mod error;
mod firecracker;
mod forward;
mod host;
mod network;
mod outcome;
mod pack;
mod vm;

pub use artifacts::{ArtifactCache, ArtifactError, pinned_artifacts};
pub use error::{BoxError, RunError};
pub use firecracker::{FirecrackerConfig, NetworkInterface, RunPaths};
pub use forward::ConnectAck;
pub use host::LocalRunHost;
pub use outcome::RunnerOutcome;
pub use pack::{Packer, PackerError};
