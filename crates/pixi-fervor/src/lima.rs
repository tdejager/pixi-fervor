//! `LimaRunHost`: boots images inside a Lima VM on macOS.
//!
//! Lima mounts the macOS home directory read-only at the same path inside the
//! VM, so the image directory, the build store and the embedded runner are
//! all reachable without copying. The runner (a Linux build of fervor-vmm)
//! imports missing layers from the build store, packs and boots the image,
//! and writes the guest outcome to a per-run file inside the VM.
//!
//! Stdio is passed straight through: on a terminal ssh allocates a pty, so
//! the console is live and Ctrl-C reaches the runner as a keystroke. Lima's
//! ssh multiplexes sessions over a persistent master, so killing the local
//! client would *not* stop the remote run; without a terminal, Ctrl-C is
//! therefore forwarded explicitly as SIGINT to the remote runner.

use std::io::IsTerminal;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::digest::ArtifactDigest;
use fervor_domain::machine::{RunOutcome, RunRequest};
use fervor_vmm::{RunError, RunnerOutcome};
use tokio::process::Command;
use tokio::signal::unix::{Signal, SignalKind, signal};

pub struct LimaRunHost {
    instance: String,
    runner: EmbeddedRunner,
    /// Build host layer store; must live under the mounted home directory.
    build_store: Utf8PathBuf,
}

impl LimaRunHost {
    pub fn new(instance: String, runner: EmbeddedRunner, build_store: Utf8PathBuf) -> Self {
        Self { instance, runner, build_store }
    }

    /// `limactl shell` without a pty, for short helper commands.
    fn helper(&self, script: &str) -> Command {
        let mut command = Command::new("limactl");
        command.args(["shell", "--tty=false", "--workdir", "/", &self.instance, "--", "sh", "-c", script]);
        command
    }

    fn runner_command(&self, runner: &Utf8Path, image_dir: &Utf8Path, build_store: &Utf8Path, run: &RemoteRun, request: &RunRequest) -> Command {
        let mut command = Command::new("limactl");
        command
            .args(["shell", "--workdir", "/", &self.instance, "--"])
            .arg(runner.as_str())
            .args(["run", image_dir.as_str(), "--source-store", build_store.as_str()])
            .args(["--outcome-file", &run.outcome_file])
            .args(["--vcpus", &request.machine.vcpus.0.to_string()])
            .args(["--memory", &request.machine.memory.0.to_string()])
            .args(["--shutdown-grace-ms", &request.shutdown_grace.as_millis().to_string()]);
        for forward in &request.forwards {
            command.args(["-p", &forward.to_string()]);
        }
        if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
            // Keep a SIGINT for our process group away from the ssh client;
            // `run` forwards it to the runner itself.
            command.process_group(0);
        }
        command
    }

    /// Reads and removes the outcome file the runner wrote for `run`.
    async fn collect(&self, run: &RemoteRun) -> Result<Option<RunnerOutcome>, RunError> {
        let report = self
            .helper(&format!("cat {0} && rm -f {0}", run.outcome_file))
            .output()
            .await
            .map_err(|e| RunError::infrastructure("reading the run outcome from Lima", e))?;
        if !report.status.success() {
            return Ok(None);
        }
        serde_json::from_slice(&report.stdout)
            .map(Some)
            .map_err(|e| RunError::infrastructure(format!("parsing {}", run.outcome_file), e))
    }

    /// Lima forwards guest listeners to the Mac over IPv4 only. If another
    /// program already serves the port — on IPv4 or IPv6, and `localhost`
    /// resolves to `::1` first — clients reach that program instead of the
    /// guest. On macOS, AirPlay Receiver serves 5000 and 7000 this way.
    fn ensure_host_ports_free(request: &RunRequest) -> Result<(), RunError> {
        const PROBE_TIMEOUT: Duration = Duration::from_millis(200);
        for forward in &request.forwards {
            let port = forward.host.port();
            let probes: Vec<SocketAddr> = if forward.host.ip().is_loopback() || forward.host.ip().is_unspecified() {
                vec![(Ipv4Addr::LOCALHOST, port).into(), (Ipv6Addr::LOCALHOST, port).into()]
            } else {
                vec![forward.host]
            };
            if let Some(taken) = probes.iter().find(|addr| TcpStream::connect_timeout(addr, PROBE_TIMEOUT).is_ok()) {
                return Err(RunError::infrastructure(
                    format!(
                        "port {port} is already served by another program on this Mac ({taken}); \
                         publish on another port, e.g. `-p 8080:{}`",
                        forward.guest_port
                    ),
                    "address in use",
                ));
            }
        }
        Ok(())
    }

    /// Boots the image in `image_dir` and waits until the guest is gone.
    pub async fn run(&self, image_dir: &Utf8Path, request: &RunRequest) -> Result<RunOutcome, RunError> {
        Self::ensure_host_ports_free(request)?;
        let mount = LimaMount::from_env()?;
        let image_dir = mount.resolve(image_dir)?;
        let build_store = mount.resolve(&self.build_store)?;
        let runner = self.runner.install(&self.build_store)?;
        let run = RemoteRun::new();

        let mut child = self.runner_command(&runner, &image_dir, &build_store, &run, request).spawn().map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::NotFound {
                "limactl not found; install it with `pixi global install -c https://prefix.dev/github-releases lima`"
            } else {
                "starting limactl"
            };
            RunError::infrastructure(hint, e)
        })?;

        let mut stop = StopSignals::install()?;
        let mut interrupted = false;
        let status = loop {
            tokio::select! {
                status = child.wait() => break status.map_err(|e| RunError::infrastructure("waiting for limactl", e))?,
                _ = stop.recv(), if !interrupted => {
                    interrupted = true;
                    // Best effort: if it fails the runner still stops when its session ends.
                    let _ = self.helper(&format!("pkill -INT -f '{}'", run.pattern())).status().await;
                }
            }
        };

        match self.collect(&run).await? {
            Some(outcome) => Ok(outcome.into()),
            None if interrupted => Err(RunError::infrastructure("interrupted before the guest was booted", format!("limactl {status}"))),
            None => Err(RunError::infrastructure("fervor-runner failed (see its output above)", format!("limactl {status}"))),
        }
    }
}

/// Signals asking `pixi fervor run` to stop. Killing the local `limactl`
/// does not end the remote session (ssh multiplexing), so each of these is
/// turned into a SIGINT for the remote runner, which shuts the guest down.
struct StopSignals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}

impl StopSignals {
    fn install() -> Result<Self, RunError> {
        let listen = |kind: SignalKind| signal(kind).map_err(|e| RunError::infrastructure("installing signal handlers", e));
        Ok(Self {
            interrupt: listen(SignalKind::interrupt())?,
            terminate: listen(SignalKind::terminate())?,
            hangup: listen(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

/// The Linux `fervor-runner` built into this binary.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedRunner(&'static [u8]);

impl EmbeddedRunner {
    pub const LINUX: Self = Self(include_bytes!(env!("FERVOR_RUNNER_BIN")));

    /// Writes the runner into `store/bin`, named by digest so a new fervor
    /// version never races an older runner. Returns its path.
    fn install(self, store: &Utf8Path) -> Result<Utf8PathBuf, RunError> {
        let digest = ArtifactDigest::of_bytes(self.0);
        let path = store.join("bin").join(format!("fervor-runner-{digest}"));
        if path.is_file() {
            return Ok(path);
        }
        let dir = path.parent().expect("runner path has a parent");
        let fail = |what: String| move |e: std::io::Error| RunError::infrastructure(what, e);
        std::fs::create_dir_all(dir).map_err(fail(format!("creating {dir}")))?;
        let staged = dir.join(format!(".fervor-runner-{}.tmp", std::process::id()));
        std::fs::write(&staged, self.0).map_err(fail(format!("writing {staged}")))?;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(fail(format!("chmod {staged}")))?;
        std::fs::rename(&staged, &path).map_err(fail(format!("installing {path}")))?;
        Ok(path)
    }
}

/// Lima mounts only the macOS home directory into the VM, at the same path.
struct LimaMount {
    home: Utf8PathBuf,
}

impl LimaMount {
    fn from_env() -> Result<Self, RunError> {
        let home = std::env::var("HOME").map_err(|e| RunError::infrastructure("reading HOME", e))?;
        Ok(Self { home: home.into() })
    }

    /// The absolute path of `path` as the VM sees it; paths outside the home
    /// directory do not exist there.
    fn resolve(&self, path: &Utf8Path) -> Result<Utf8PathBuf, RunError> {
        let absolute = path.canonicalize_utf8().map_err(|e| RunError::infrastructure(format!("resolving {path}"), e))?;
        if absolute.starts_with(&self.home) {
            Ok(absolute)
        } else {
            Err(RunError::infrastructure(
                format!("{absolute} is outside {}", self.home),
                "Lima only mounts your home directory; move the image or cache there",
            ))
        }
    }
}

/// Identifies one run inside the Lima VM.
struct RemoteRun {
    id: String,
    outcome_file: String,
}

impl RemoteRun {
    fn new() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let id = format!("{}-{nanos}", std::process::id());
        let outcome_file = format!("/tmp/fervor-run-{id}.json");
        Self { id, outcome_file }
    }

    /// `pkill -f` regex for this run's runner. The bracket keeps the pattern
    /// from matching the shell command line that carries it.
    fn pattern(&self) -> String {
        format!("[f]ervor-run-{}", self.id)
    }
}
