//! Layer file systems: writes file sets into SquashFS images and snapshots
//! host directories into file sets.

mod builder;
mod error;
mod host_tree;

pub use builder::SquashfsLayerBuilder;
pub use error::{BoxError, BuildError, HostTreeError};
pub use host_tree::HostTreeReader;
