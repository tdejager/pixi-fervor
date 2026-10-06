//! `fervor-runner run <IMAGE_DIR>`: boots a built image on this machine.
//!
//! Stdout is the guest's serial console. With `--outcome-file`, the guest's
//! outcome is also written there as a [`RunnerOutcome`] for remote callers.

use std::num::{NonZeroU8, NonZeroU32};
use std::time::Duration;

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use fervor_domain::machine::{MachineSpec, MemoryMib, PortForward, RunRequest, VcpuCount};
use fervor_store::{ImageDir, LayerStore};
use fervor_vmm::{LocalRunHost, RunnerOutcome};
use miette::{IntoDiagnostic, WrapErr};

#[derive(Parser)]
#[command(version, about = "Boots fervor images in Firecracker")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot an image and wait until its entrypoint exits.
    Run(RunArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Directory containing the image's manifest.json.
    image_dir: Utf8PathBuf,
    /// Layer store to import missing layer blobs from (e.g. the build host's, mounted).
    #[arg(long)]
    source_store: Option<Utf8PathBuf>,
    /// Local store for layers, packs and artifacts.
    #[arg(long, default_value_t = LayerStore::default_root())]
    cache_dir: Utf8PathBuf,
    #[command(flatten)]
    request: RequestArgs,
    /// Write the guest outcome as JSON to this file when the run ends.
    #[arg(long)]
    outcome_file: Option<Utf8PathBuf>,
}

/// The arguments that make up the [`RunRequest`].
#[derive(clap::Args)]
struct RequestArgs {
    #[arg(long, default_value_t = VcpuCount::default().0)]
    vcpus: NonZeroU8,
    /// Guest memory in MiB.
    #[arg(long, default_value_t = MemoryMib::default().0)]
    memory: NonZeroU32,
    /// Forward a host TCP address to a guest port: `HOST_PORT:GUEST_PORT` or `HOST_IP:HOST_PORT:GUEST_PORT`.
    #[arg(short = 'p', long = "publish", value_name = "HOST:GUEST")]
    forwards: Vec<PortForward>,
    /// Time the entrypoint gets between SIGTERM and SIGKILL on shutdown.
    #[arg(long, default_value_t = 10_000)]
    shutdown_grace_ms: u64,
}

impl RequestArgs {
    fn into_request(self) -> RunRequest {
        RunRequest {
            machine: MachineSpec::new(VcpuCount(self.vcpus), MemoryMib(self.memory)),
            forwards: self.forwards,
            shutdown_grace: Duration::from_millis(self.shutdown_grace_ms),
        }
    }
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let Command::Run(args) = Cli::parse().command;
    let image = ImageDir::new(args.image_dir.clone())
        .load()
        .into_diagnostic()
        .wrap_err_with(|| format!("loading the image in {}", args.image_dir))?;
    let request = args.request.into_request();
    let store = LayerStore::open(&args.cache_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("opening the store at {}", args.cache_dir))?;
    let host = LocalRunHost::new(store, args.source_store);

    let outcome = host
        .run(&image, &request)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("running image {}", image.id()))?;

    eprintln!("fervor-runner: {outcome}");
    if let Some(path) = &args.outcome_file {
        RunnerOutcome::from(&outcome)
            .write_to(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("writing the outcome to {path}"))?;
    }
    std::process::exit(outcome.exit.exit_code());
}
