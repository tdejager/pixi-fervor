//! conda support: [`RattlerResolver`] solves environment specs against conda
//! channels and [`RattlerPackageContents`] turns package archives into the
//! files they install, without ever unpacking them onto the host filesystem.

mod archive;
mod client;
mod contents;
mod download;
mod error;
mod install;
mod resolver;
#[cfg(test)]
mod tests;

pub use client::{ClientError, CondaClient};
pub use contents::RattlerPackageContents;
pub use error::{BoxError, ContentsError, ResolveError};
pub use install::InstallTarget;
pub use resolver::RattlerResolver;
