use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::image::Image;
use fervor_domain::machine::{RunOutcome, RunRequest};
use fervor_store::LayerStore;
use fervor_vmm::{LocalRunHost, RunError};
use miette::IntoDiagnostic;

use crate::cli::RunHostArg;
#[cfg(target_os = "macos")]
use crate::lima::{EmbeddedRunner, LimaRunHost};

/// Where an image is booted: this Linux machine, or a Lima VM on macOS.
pub enum RunHost {
    Local(LocalRunHost),
    #[cfg(target_os = "macos")]
    Lima(LimaRunHost),
}

impl RunHost {
    pub fn new(arg: &RunHostArg, cache_root: Utf8PathBuf) -> miette::Result<Self> {
        match arg {
            RunHostArg::Local => {
                let store = LayerStore::open(cache_root).into_diagnostic()?;
                Ok(Self::Local(LocalRunHost::new(store, None)))
            }
            #[cfg(target_os = "macos")]
            RunHostArg::Lima { instance } => Ok(Self::Lima(LimaRunHost::new(instance.clone(), EmbeddedRunner::LINUX, cache_root))),
            #[cfg(not(target_os = "macos"))]
            RunHostArg::Lima { .. } => Err(miette::miette!("the Lima run host is only available on macOS; use --run-host local")),
        }
    }

    /// Boots `image` (loaded from `image_dir`) and waits until the guest is gone.
    #[cfg_attr(not(target_os = "macos"), expect(unused_variables, reason = "only the Lima host needs the directory"))]
    pub async fn run(&self, image_dir: &Utf8Path, image: &Image, request: &RunRequest) -> Result<RunOutcome, RunError> {
        match self {
            Self::Local(host) => host.run(image, request).await,
            #[cfg(target_os = "macos")]
            Self::Lima(host) => host.run(image_dir, request).await,
        }
    }
}
