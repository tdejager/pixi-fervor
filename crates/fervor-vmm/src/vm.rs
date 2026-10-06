//! One Firecracker process: per-run directory, control channel, port
//! forwards, signal handling.

use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::machine::{GuestExit, RunOutcome, RunRequest};
use fervor_guest_abi::{ExitReport, GuestToHost, HostToGuest};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::error::RunError;
use crate::firecracker::{FirecrackerConfig, RunPaths};
use crate::forward::{GuestConnector, PortForwarder};
use crate::outcome::guest_exit;

/// How long Firecracker may outlive the shutdown grace period before it is killed.
const KILL_MARGIN: Duration = Duration::from_secs(5);
/// How long to wait for buffered control messages once Firecracker is gone.
const CONTROL_DRAIN: Duration = Duration::from_secs(2);

pub(crate) struct VmFiles<'a> {
    pub firecracker: &'a Utf8Path,
    pub kernel: &'a Utf8Path,
    pub boot_layer: &'a Utf8Path,
    pub pack: &'a Utf8Path,
}

/// The per-run directory holding Firecracker's config, log and sockets;
/// removed when dropped.
struct RunDir {
    _dir: TempDir,
    paths: RunPaths,
}

impl RunDir {
    /// Creates the directory with the config for `files` and an empty log.
    fn create(files: &VmFiles<'_>, request: &RunRequest) -> Result<Self, RunError> {
        let dir = tempfile::Builder::new()
            .prefix("fervor-run-")
            .tempdir()
            .map_err(|e| RunError::infrastructure("creating the run directory", e))?;
        let path = Utf8PathBuf::from_path_buf(dir.path().to_owned()).map_err(|p| {
            RunError::infrastructure("creating the run directory", io::Error::other(format!("{} is not UTF-8", p.display())))
        })?;
        let paths = RunPaths::new(path);

        let config = FirecrackerConfig::new(files.kernel, files.boot_layer, files.pack, &request.machine, &paths);
        std::fs::write(paths.config(), config.to_json())
            .map_err(|e| RunError::infrastructure(format!("writing {}", paths.config()), e))?;
        std::fs::File::create(paths.log()).map_err(|e| RunError::infrastructure(format!("creating {}", paths.log()), e))?;
        Ok(Self { _dir: dir, paths })
    }

    /// Firecracker exited on its own before the guest connected; its log says why.
    fn firecracker_failed(&self, status: ExitStatus) -> RunError {
        let log = std::fs::read_to_string(self.paths.log()).unwrap_or_default();
        let detail = if log.trim().is_empty() { String::new() } else { format!(":\n{}", log.trim_end()) };
        RunError::infrastructure("firecracker failed", io::Error::other(format!("firecracker exited with {status}{detail}")))
    }
}

/// One boot, prepared up to starting Firecracker: the run directory and
/// every host-side listener bound.
pub(crate) struct VmSession<'a> {
    firecracker: &'a Utf8Path,
    shutdown_grace: Duration,
    signals: Signals,
    run_dir: RunDir,
    control: UnixListener,
    forwarders: Vec<PortForwarder>,
}

impl<'a> VmSession<'a> {
    pub(crate) async fn prepare(files: VmFiles<'a>, request: &RunRequest) -> Result<Self, RunError> {
        // Before anything needs cleaning up: from here on a signal is handled
        // (pending ones are seen by `run`), not fatal.
        let signals = Signals::install()?;
        let run_dir = RunDir::create(&files, request)?;
        let paths = &run_dir.paths;

        let control = UnixListener::bind(paths.control_socket())
            .map_err(|e| RunError::infrastructure(format!("binding {}", paths.control_socket()), e))?;
        let guest = GuestConnector::new(paths.vsock());
        let mut forwarders = Vec::with_capacity(request.forwards.len());
        for fwd in &request.forwards {
            let listener = TcpListener::bind(fwd.host)
                .await
                .map_err(|e| RunError::infrastructure(format!("binding {} for -p {fwd}", fwd.host), e))?;
            forwarders.push(PortForwarder::new(listener, fwd.guest_port, guest.clone()));
        }

        Ok(Self { firecracker: files.firecracker, shutdown_grace: request.shutdown_grace, signals, run_dir, control, forwarders })
    }

    /// Starts Firecracker and serves the guest until the VM stops.
    pub(crate) async fn run(self) -> Result<RunOutcome, RunError> {
        let started = Instant::now();
        let mut child = self.spawn_firecracker()?;
        let Self { shutdown_grace, mut signals, run_dir, control, forwarders, .. } = self;
        let forwards: Vec<_> = forwarders.into_iter().map(|forwarder| tokio::spawn(forwarder.serve())).collect();
        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let mut control = tokio::spawn(ControlSession { listener: control, events: events_tx, shutdown: shutdown_rx }.serve());

        let mut state = ControlState::default();
        let mut kill_at: Option<Instant> = None;
        let status = loop {
            tokio::select! {
                status = child.wait() => break status.map_err(|e| RunError::infrastructure("waiting for firecracker", e))?,
                Some(event) = events.recv() => state.apply(event),
                name = signals.recv() => {
                    if kill_at.is_some() {
                        continue;
                    }
                    let grace_ms = u64::try_from(shutdown_grace.as_millis()).unwrap_or(u64::MAX);
                    if state.connected && shutdown_tx.send(grace_ms).is_ok() {
                        tracing::info!("{name}: asking the guest to shut down");
                        kill_at = Some(Instant::now() + shutdown_grace + KILL_MARGIN);
                    } else {
                        tracing::warn!("{name}: guest has no control connection, killing the VM");
                        kill_at = Some(Instant::now());
                        let _ = child.start_kill();
                    }
                }
                () = tokio::time::sleep_until(kill_at.unwrap_or_else(Instant::now)), if kill_at.is_some_and(|at| at > Instant::now()) => {
                    tracing::warn!("guest still running after the shutdown grace period, killing the VM");
                    let _ = child.start_kill();
                }
            }
        };
        let wall_time = started.elapsed();
        for task in forwards {
            task.abort();
        }

        // The guest sends `Exited` right before rebooting; Firecracker closing the
        // socket ends the session after the buffered lines are read.
        state.drain(&mut events);
        if state.connected {
            let _ = tokio::time::timeout(CONTROL_DRAIN, &mut control).await;
        }
        control.abort();
        state.drain(&mut events);

        // A VMM that dies on its own before the guest ever connects is broken
        // (bad config, missing device); one we killed is just a stopped VM.
        let shutdown_requested = kill_at.is_some();
        if !state.connected && !shutdown_requested && !status.success() {
            return Err(run_dir.firecracker_failed(status));
        }
        Ok(RunOutcome { exit: state.into_exit(status), wall_time })
    }

    fn spawn_firecracker(&self) -> Result<Child, RunError> {
        let mut command = Command::new(self.firecracker);
        command
            .args(FirecrackerConfig::cli_args(&self.run_dir.paths.config()))
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            // A terminal's Ctrl-C must reach only us; we shut the guest down.
            .process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe and touches no shared state.
        unsafe {
            command.pre_exec(|| {
                rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL)).map_err(io::Error::from)
            });
        }
        command.spawn().map_err(|e| RunError::infrastructure(format!("starting {}", self.firecracker), e))
    }
}

enum ControlEvent {
    Connected,
    Ready,
    Exited(ExitReport),
}

/// What the host has learned from the guest over the control channel.
#[derive(Default)]
struct ControlState {
    connected: bool,
    report: Option<ExitReport>,
}

impl ControlState {
    fn apply(&mut self, event: ControlEvent) {
        match event {
            ControlEvent::Connected => self.connected = true,
            ControlEvent::Ready => tracing::info!("guest entrypoint started"),
            ControlEvent::Exited(report) => self.report = Some(report),
        }
    }

    /// Applies every event already queued.
    fn drain(&mut self, events: &mut mpsc::UnboundedReceiver<ControlEvent>) {
        while let Ok(event) = events.try_recv() {
            self.apply(event);
        }
    }

    fn into_exit(self, status: ExitStatus) -> GuestExit {
        match self.report {
            Some(report) => guest_exit(report),
            None => {
                tracing::warn!("VM stopped ({status}) without an exit report from the guest");
                GuestExit::NoReport
            }
        }
    }
}

/// The single guest-initiated control connection: guest messages become
/// [`ControlEvent`]s, shutdown requests (grace in ms) are sent to the guest.
struct ControlSession {
    listener: UnixListener,
    events: mpsc::UnboundedSender<ControlEvent>,
    shutdown: mpsc::UnboundedReceiver<u64>,
}

impl ControlSession {
    async fn serve(mut self) {
        let stream = match self.listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!("accepting the guest control connection: {e}");
                return;
            }
        };
        let _ = self.events.send(ControlEvent::Connected);
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => match serde_json::from_str::<GuestToHost>(&line) {
                        Ok(GuestToHost::Ready) => { let _ = self.events.send(ControlEvent::Ready); }
                        Ok(GuestToHost::Exited { report }) => { let _ = self.events.send(ControlEvent::Exited(report)); }
                        Err(e) => tracing::warn!("ignoring malformed control message `{line}`: {e}"),
                    },
                    Ok(None) => return,
                    Err(e) => {
                        tracing::warn!("reading the guest control connection: {e}");
                        return;
                    }
                },
                Some(grace_ms) = self.shutdown.recv() => {
                    let mut message = serde_json::to_vec(&HostToGuest::Shutdown { grace_ms }).expect("control message serializes");
                    message.push(b'\n');
                    if let Err(e) = writer.write_all(&message).await {
                        tracing::warn!("sending shutdown to the guest: {e}");
                    }
                }
            }
        }
    }
}

struct Signals {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}

impl Signals {
    fn install() -> Result<Self, RunError> {
        let install = |kind| signal(kind).map_err(|e| RunError::infrastructure("installing signal handlers", e));
        Ok(Self {
            interrupt: install(SignalKind::interrupt())?,
            terminate: install(SignalKind::terminate())?,
            hangup: install(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.hangup.recv() => "SIGHUP",
        }
    }
}
