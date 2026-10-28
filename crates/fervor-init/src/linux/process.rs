//! Starting the entrypoint, services and journald with the image's
//! environment, and signalling them through pidfds.

use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use fervor_guest_abi::ExitReport;
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, WaitStatus};

use super::journal;
use super::{Context, Error};
use crate::plan::{SearchPath, StreamPriority};

/// What every supervised process shares: the image's complete environment
/// and working directory.
pub struct Environment {
    pub env: Vec<(String, String)>,
    pub workdir: String,
}

/// Where a process's stdout and stderr go.
pub enum Output {
    /// Inherited from init: the serial console.
    Console,
    /// Two journal streams under this identifier.
    Journal { identifier: String },
}

/// A resolved command, ready to be spawned (again).
pub struct Launch {
    /// For messages: `the entrypoint`, `service web`.
    label: String,
    program: PathBuf,
    argv: Vec<String>,
    /// Only the entrypoint reads the console; services get `/dev/null`.
    console_stdin: bool,
    output: Output,
}

impl Launch {
    /// Resolves `argv[0]` along the environment's `PATH`.
    pub fn resolve(
        label: String,
        argv: &[String],
        environment: &Environment,
        console_stdin: bool,
        output: Output,
    ) -> Result<Self, Error> {
        let Some(argv0) = argv.first() else {
            return Err(Error::msg(format!("{label} has an empty argv")));
        };
        let search_path = SearchPath::from_env(&environment.env);
        let program = search_path.resolve_executable(argv0).ok_or_else(|| {
            Error::msg(format!(
                "{label} `{argv0}` not found in PATH ({search_path})"
            ))
        })?;
        Ok(Self {
            label,
            program,
            argv: argv.to_vec(),
            console_stdin,
            output,
        })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn spawn(&self, environment: &Environment) -> Result<Process, Error> {
        let (argv0, args) = self
            .argv
            .split_first()
            .expect("resolve rejects an empty argv");
        let mut command = Command::new(&self.program);
        command
            .arg0(argv0)
            .args(args)
            .env_clear()
            .envs(environment.env.iter().map(|(name, value)| (name, value)))
            .current_dir(&environment.workdir);
        // std keeps the parent's signal mask, and init blocks SIGCHLD for its
        // signalfd; children must start with nothing blocked.
        // SAFETY: sigemptyset and sigprocmask are async-signal-safe and touch
        // only the child's own state.
        unsafe {
            command.pre_exec(|| {
                let mut empty: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut empty);
                if libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if !self.console_stdin {
            command.stdin(Stdio::null());
        }
        if let Output::Journal { identifier } = &self.output {
            command
                .stdout(journal::stream(identifier, StreamPriority::Stdout)?)
                .stderr(journal::stream(identifier, StreamPriority::Stderr)?);
        }
        let child = command.spawn().context(|| {
            format!(
                "starting {} {} in {}",
                self.label,
                self.program.display(),
                environment.workdir
            )
        })?;
        let pid = Pid::from_child(&child);
        // Reaping happens in the supervisor through wait(2), never via `child`.
        drop(child);

        let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())
            .context(|| format!("opening a pidfd for {}", self.label))?;
        Ok(Process { pid, pidfd })
    }
}

/// A running (or not yet reaped) child.
pub struct Process {
    pid: Pid,
    /// Signals through it can never hit a recycled pid.
    pidfd: OwnedFd,
}

impl Process {
    pub fn pid(&self) -> Pid {
        self.pid
    }

    pub fn signal(&self, signal: Signal, label: &str) {
        match rustix::process::pidfd_send_signal(&self.pidfd, signal) {
            // ESRCH: it has exited and only awaits reaping.
            Ok(()) | Err(Errno::SRCH) => {}
            Err(err) => eprintln!("fervor-init: sending {signal:?} to {label} failed: {err}"),
        }
    }
}

/// The host's view of how a process ended.
pub fn report(status: WaitStatus) -> ExitReport {
    match status.terminating_signal() {
        Some(signal) => ExitReport::Signaled { signal },
        None => ExitReport::Exited {
            code: status.exit_status().unwrap_or_default(),
        },
    }
}

/// `exit code 1`, `signal 9`, for console messages.
pub fn describe(status: WaitStatus) -> String {
    match report(status) {
        ExitReport::Signaled { signal } => format!("signal {signal}"),
        ExitReport::Exited { code } => format!("exit code {code}"),
        ExitReport::InitFailed { message } => message,
    }
}

impl SearchPath<'_> {
    /// Resolves `program` against the guest filesystem.
    fn resolve_executable(&self, program: &str) -> Option<PathBuf> {
        self.resolve(program, Self::is_executable)
    }

    fn is_executable(path: &Path) -> bool {
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
}
