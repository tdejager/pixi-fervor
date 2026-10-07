//! `pixi fervor`: parses arguments and hands them to [`Fervor`], which owns
//! the cache location and runs the build and run commands.

mod cli;
#[cfg(target_os = "macos")]
mod lima;
mod progress;
mod run_host;

use std::fmt;

use camino::Utf8PathBuf;
use clap::Parser;
use fervor_app::ImageBuilder;
use fervor_conda::CondaClient;
use fervor_domain::boot::InitBinary;
use fervor_domain::image::Image;
use fervor_domain::platform::GuestPlatform;
use fervor_domain::size::ByteSize;
use fervor_store::{ImageDir, LayerStore};
use miette::IntoDiagnostic;

use crate::cli::{BuildArgs, Cli, Command, RunArgs};
use crate::progress::StderrProgress;
use crate::run_host::RunHost;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> miette::Result<()> {
    let cli = Cli::parse();
    let fervor = Fervor { cache_root: cli.cache_dir.unwrap_or_else(LayerStore::default_root) };
    match cli.command {
        Command::Build(args) => fervor.build(args).await,
        Command::Run(args) => std::process::exit(fervor.run(args).await?),
    }
}

/// The `pixi fervor` commands, sharing one cache root.
struct Fervor {
    cache_root: Utf8PathBuf,
}

impl Fervor {
    /// PID 1 of every guest, cross-compiled for `platform`.
    fn init_binary(platform: GuestPlatform) -> InitBinary {
        let bytes: &[u8] = match platform {
            GuestPlatform::LinuxAarch64 => include_bytes!(env!("FERVOR_INIT_AARCH64_BIN")),
            GuestPlatform::LinuxX86_64 => include_bytes!(env!("FERVOR_INIT_X86_64_BIN")),
        };
        InitBinary::new(bytes.to_vec())
    }

    async fn build(&self, args: BuildArgs) -> miette::Result<()> {
        let command = args.into_command()?;
        let output = command.output.clone();
        let init = Self::init_binary(command.environment.platform());
        let client = CondaClient::authenticated().into_diagnostic()?;
        let builder = ImageBuilder::new(&self.cache_root, client, init).into_diagnostic()?;
        let image = builder.build(command, &StderrProgress::report).await.into_diagnostic()?;
        eprintln!("{}", ImageSummary { image: &image, output: &output });
        Ok(())
    }

    /// Returns the process exit code: the guest entrypoint's.
    async fn run(&self, args: RunArgs) -> miette::Result<i32> {
        let image = ImageDir::new(&args.image).load().into_diagnostic()?;
        let host = RunHost::new(&args.run_host, self.cache_root.clone())?;
        let request = args.request();
        // The guest prints its own port (e.g. Flask's `Running on …:5000`); say where it is reachable here.
        for forward in &request.forwards {
            eprintln!("publishing {} → guest port {}", forward.host, forward.guest_port);
        }
        let outcome = host.run(&args.image, &image, &request).await.into_diagnostic()?;
        eprintln!("{outcome}");
        Ok(outcome.exit.exit_code())
    }
}

/// `image <id> → out/manifest.json (10 layers, 87.4 MiB)`
struct ImageSummary<'a> {
    image: &'a Image,
    output: &'a Utf8PathBuf,
}

impl fmt::Display for ImageSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = ByteSize::bytes(self.image.all_layers().map(|l| l.size.as_u64()).sum());
        let layers = self.image.layers().layers().len() + 1;
        write!(f, "image {} → {}/manifest.json ({layers} layers, {total})", self.image.id(), self.output)
    }
}
