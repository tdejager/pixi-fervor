use std::num::{NonZeroU8, NonZeroU32};
use std::str::FromStr;
use std::time::Duration;

use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand};
use fervor_app::{BuildImage, HostTreeMount};
use fervor_domain::entrypoint::{Argv, EnvName, EnvVars};
use fervor_domain::environment::EnvironmentSpec;
use fervor_domain::image::ScratchSize;
use fervor_domain::layer::{ConflictPolicy, SizeThresholdPolicy};
use fervor_domain::machine::{MachineSpec, MemoryMib, PortForward, RunRequest, VcpuCount};
use fervor_domain::nonempty::NonEmpty;
use fervor_domain::path::GuestPath;
use fervor_domain::platform::{GuestLibc, GuestPlatform};
use fervor_domain::size::ByteSize;
use fervor_vmm::pinned_artifacts;
use miette::{IntoDiagnostic, WrapErr, miette};
use rattler_conda_types::{Channel, ChannelConfig, MatchSpec, ParseStrictness};

/// Build and run Firecracker microVM images from conda environments.
#[derive(Debug, Parser)]
#[command(name = "pixi-fervor", bin_name = "pixi fervor", version)]
pub struct Cli {
    /// Layer store and download cache [default: $FERVOR_CACHE_DIR, else $XDG_CACHE_HOME/fervor or ~/.cache/fervor]
    #[arg(long, global = true)]
    pub cache_dir: Option<Utf8PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Resolve packages, build layers and write an image manifest.
    Build(BuildArgs),
    /// Boot an image and wait for its entrypoint to exit.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct BuildArgs {
    /// Package requirement (conda match spec), e.g. `python=3.12`.
    #[arg(short = 's', long = "spec", value_name = "SPEC", required = true)]
    pub specs: Vec<String>,

    /// Channel name or URL, highest priority first.
    #[arg(short = 'c', long = "channel", value_name = "CHANNEL", default_value = "conda-forge")]
    pub channels: Vec<String>,

    /// Guest platform.
    #[arg(long, default_value_t = BuildArgs::host_platform())]
    pub platform: GuestPlatform,

    /// Environment variable for the entrypoint.
    #[arg(short = 'e', long = "env", value_name = "NAME=VALUE")]
    pub env: Vec<EnvAssignment>,

    /// Working directory of the entrypoint.
    #[arg(long, default_value = "/")]
    pub workdir: GuestPath,

    /// Copy a host directory into the image (on top of the packages).
    #[arg(long = "copy", value_name = "SRC:DEST")]
    pub copies: Vec<CopySpec>,

    /// Packages below this archive size share one layer.
    #[arg(long, default_value = "1MiB")]
    pub small_layer_threshold: ByteSize,

    /// Let the upper layer win when layers provide the same path.
    #[arg(long)]
    pub allow_conflicts: bool,

    /// Size limit (MiB) of the guest's writable scratch layer.
    #[arg(long, default_value_t = NonZeroU32::new(1024).expect("non-zero"))]
    pub scratch_size_mib: NonZeroU32,

    /// Image directory to write `manifest.json` to.
    #[arg(short, long)]
    pub output: Utf8PathBuf,

    /// Entrypoint program and arguments.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Image directory produced by `build`.
    pub image: Utf8PathBuf,

    /// Forward a host TCP port to a guest port: `HOST_PORT:GUEST_PORT` or `HOST_IP:HOST_PORT:GUEST_PORT`.
    #[arg(short = 'p', long = "publish", value_name = "PORTS")]
    pub forwards: Vec<PortForward>,

    #[arg(long, default_value_t = NonZeroU8::new(2).expect("non-zero"))]
    pub vcpus: NonZeroU8,

    /// Guest memory in MiB.
    #[arg(long, default_value_t = NonZeroU32::new(512).expect("non-zero"))]
    pub memory: NonZeroU32,

    /// Where to boot: `local` (this Linux machine) or `lima[:INSTANCE]`.
    #[arg(long, default_value_t = RunHostArg::default())]
    pub run_host: RunHostArg,

    /// Seconds between SIGTERM and SIGKILL when stopping the guest.
    #[arg(long, default_value_t = 10)]
    pub shutdown_grace_secs: u64,
}

impl BuildArgs {
    /// The guest architecture matching this machine, so guests run without emulation.
    fn host_platform() -> GuestPlatform {
        match std::env::consts::ARCH {
            "x86_64" => GuestPlatform::LinuxX86_64,
            _ => GuestPlatform::LinuxAarch64,
        }
    }

    pub fn into_command(self) -> miette::Result<BuildImage> {
        let artifacts = pinned_artifacts(self.platform).into_diagnostic()?;
        let channel_config = ChannelConfig::default_with_root_dir(std::env::current_dir().into_diagnostic()?);
        let specs = self
            .specs
            .iter()
            .map(|s| MatchSpec::from_str(s, ParseStrictness::Lenient).into_diagnostic().wrap_err_with(|| format!("parsing `{s}`")))
            .collect::<miette::Result<Vec<_>>>()?;
        let channels = self
            .channels
            .iter()
            .map(|c| Channel::from_str(c, &channel_config).into_diagnostic().wrap_err_with(|| format!("parsing channel `{c}`")))
            .collect::<miette::Result<Vec<_>>>()?;
        let environment = EnvironmentSpec::new(
            NonEmpty::new(specs).ok_or_else(|| miette!("at least one --spec is required"))?,
            NonEmpty::new(channels).ok_or_else(|| miette!("at least one --channel is required"))?,
            self.platform,
            GuestLibc::default(),
            artifacts.kernel_version(),
        );
        Ok(BuildImage {
            environment,
            artifacts,
            argv: Argv::new(NonEmpty::new(self.command).ok_or_else(|| miette!("an entrypoint command is required after `--`"))?),
            env: self.env.into_iter().map(|a| (a.name, a.value)).collect::<EnvVars>(),
            workdir: self.workdir,
            host_trees: self.copies.into_iter().map(|c| HostTreeMount { source: c.source, mount_at: c.dest }).collect(),
            policy: SizeThresholdPolicy { threshold: self.small_layer_threshold },
            conflicts: if self.allow_conflicts { ConflictPolicy::UpperLayerWins } else { ConflictPolicy::Reject },
            scratch: ScratchSize(self.scratch_size_mib),
            output: self.output,
        })
    }
}

impl RunArgs {
    pub fn request(&self) -> RunRequest {
        RunRequest {
            machine: MachineSpec::new(VcpuCount(self.vcpus), MemoryMib(self.memory)),
            forwards: self.forwards.clone(),
            shutdown_grace: Duration::from_secs(self.shutdown_grace_secs),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnvAssignment {
    pub name: EnvName,
    pub value: String,
}

impl FromStr for EnvAssignment {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, value) = s.split_once('=').ok_or_else(|| format!("`{s}` is not NAME=VALUE"))?;
        Ok(Self { name: name.parse().map_err(|e| format!("{e}"))?, value: value.to_owned() })
    }
}

#[derive(Debug, Clone)]
pub struct CopySpec {
    pub source: Utf8PathBuf,
    pub dest: GuestPath,
}

impl FromStr for CopySpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (source, dest) = s.rsplit_once(':').ok_or_else(|| format!("`{s}` is not SRC:DEST"))?;
        Ok(Self { source: source.into(), dest: dest.parse().map_err(|e| format!("{e}"))? })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunHostArg {
    Local,
    Lima { instance: String },
}

impl Default for RunHostArg {
    /// macOS has no KVM: default to the `fervor` Lima instance there.
    fn default() -> Self {
        if cfg!(target_os = "macos") { Self::Lima { instance: "fervor".to_owned() } } else { Self::Local }
    }
}

impl std::fmt::Display for RunHostArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("local"),
            Self::Lima { instance } => write!(f, "lima:{instance}"),
        }
    }
}

impl FromStr for RunHostArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.split_once(':') {
            None if s == "local" => Ok(Self::Local),
            None if s == "lima" => Ok(Self::Lima { instance: "fervor".to_owned() }),
            Some(("lima", instance)) if !instance.is_empty() => Ok(Self::Lima { instance: instance.to_owned() }),
            _ => Err(format!("`{s}` is not a run host (expected `local`, `lima` or `lima:INSTANCE`)")),
        }
    }
}
