//! fervor's domain model.
//!
//! Pure types and rules — no I/O. The adapter crates (conda, layerfs, store,
//! vmm) do the I/O on these types; rattler's conda *data* types (`MatchSpec`,
//! `Subdir`, `PackageName`, `RepoDataRecord`, …) are used directly as the
//! ubiquitous language of conda.
//!
//! Model modules:
//! - [`environment`]: which exact packages make up a guest
//! - [`layer`] / [`files`] / [`boot`]: how files become content-addressed layers
//! - [`image`] / [`manifest`]: the aggregate root that gets built, moved and booted
//! - [`machine`]: how an image is run and what happened

pub mod boot;
pub mod digest;
pub mod entrypoint;
pub mod environment;
pub mod files;
pub mod image;
pub mod layer;
pub mod machine;
pub mod manifest;
pub mod nonempty;
pub mod path;
pub mod platform;
pub mod size;
