//! The build use case: [`ImageBuilder`] ties resolving, package contents,
//! layer building and the layer store together. [`RunLayerBuilder`] turns
//! run-time files into layers the same way.

mod build_image;
mod run_layers;

pub use build_image::{BuildEvent, BuildImage, BuildImageError, HostTreeMount, ImageBuilder};
pub use run_layers::{RunFile, RunLayerBuilder, RunLayerError};
