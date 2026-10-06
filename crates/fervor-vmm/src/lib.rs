//! Boots fervor images in Firecracker on a Linux host with KVM.
//!
//! - [`pinned_artifacts`]: selects the Firecracker and guest kernel releases
//! - [`ArtifactCache`]: downloads and verifies the pinned Firecracker binary and guest kernel
//! - [`Packer`]: concatenates an image's layers behind a [`PackHeader`](fervor_guest_abi::PackHeader)
//! - [`FirecrackerConfig`]: the `--config-file` handed to Firecracker
//! - [`LocalRunHost`]: imports an image's layers, packs them and boots the VM
//! - [`RunnerOutcome`]: the outcome line `fervor-runner` prints for a remote caller

mod artifacts;
mod error;
mod firecracker;
mod forward;
mod host;
mod outcome;
mod pack;
mod vm;

pub use artifacts::{ArtifactCache, ArtifactError, pinned_artifacts};
pub use error::{BoxError, RunError};
pub use firecracker::{FirecrackerConfig, RunPaths};
pub use forward::ConnectAck;
pub use host::LocalRunHost;
pub use outcome::RunnerOutcome;
pub use pack::{Packer, PackerError};
