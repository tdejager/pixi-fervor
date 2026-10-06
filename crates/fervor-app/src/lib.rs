//! The build use case: [`ImageBuilder`] ties resolving, package contents,
//! layer building and the layer store together.

mod build_image;

pub use build_image::{BuildEvent, BuildImage, BuildImageError, HostTreeMount, ImageBuilder};
