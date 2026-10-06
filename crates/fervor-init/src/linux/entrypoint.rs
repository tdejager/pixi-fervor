//! Spawning the entrypoint, delivering shutdown signals to it and reaping
//! every child (the PID 1 duty) until it is gone.

use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use fervor_guest_abi::{ExitReport, GuestConfig};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags, Signal, WaitOptions};

use super::{Context, Error};
use crate::plan::SearchPath;

#[derive(Default)]
pub struct Supervisor {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// pidfd of the entrypoint: signals through it can never hit a recycled pid.
    entrypoint: Option<OwnedFd>,
    /// A shutdown that arrived before the entrypoint was spawned.
    pending_shutdown: Option<Duration>,
}

impl Supervisor {
    pub fn spawn(self: &Arc<Self>, config: &GuestConfig) -> Result<Pid, Error> {
        let Some((argv0, args)) = config.argv.split_first() else {
            return Err(Error::msg("the image has an empty entrypoint argv"));
        };
        let search_path = SearchPath::from_env(&config.env);
        let program = search_path.resolve_executable(argv0).ok_or_else(|| {
            Error::msg(format!(
                "entrypoint `{argv0}` not found in PATH ({search_path})"
            ))
        })?;

        let child = Command::new(&program)
            .arg0(argv0)
            .args(args)
            .env_clear()
            .envs(config.env.iter().map(|(name, value)| (name, value)))
            .current_dir(&config.workdir)
            .spawn()
            .context(|| {
                format!(
                    "starting the entrypoint {} in {}",
                    program.display(),
                    config.workdir
                )
            })?;
        let pid = Pid::from_child(&child);
        // Reaping happens in `wait_for_exit` through wait(2), never via `child`.
        drop(child);

        let pidfd = rustix::process::pidfd_open(pid, PidfdFlags::empty())
            .context(|| "opening a pidfd for the entrypoint".to_owned())?;
        let mut state = self.lock();
        state.entrypoint = Some(pidfd);
        if let Some(grace) = state.pending_shutdown.take() {
            drop(state);
            self.request_shutdown(grace);
        }
        Ok(pid)
    }

    /// SIGTERM now, SIGKILL once `grace` has passed.
    pub fn request_shutdown(self: &Arc<Self>, grace: Duration) {
        let mut state = self.lock();
        if state.entrypoint.is_none() {
            state.pending_shutdown = Some(grace);
            return;
        }
        state.signal(Signal::TERM);
        drop(state);
        let supervisor = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(grace);
            supervisor.lock().signal(Signal::KILL);
        });
    }

    /// Reaps children until the entrypoint (`pid`) has terminated.
    pub fn wait_for_exit(&self, pid: Pid) -> ExitReport {
        loop {
            match rustix::process::wait(WaitOptions::empty()) {
                Ok(Some((reaped, status))) if reaped == pid => {
                    if let Some(signal) = status.terminating_signal() {
                        return ExitReport::Signaled { signal };
                    }
                    return ExitReport::Exited {
                        code: status.exit_status().unwrap_or_default(),
                    };
                }
                Ok(_) | Err(Errno::INTR) => {}
                Err(err) => {
                    let message = format!("waiting for the entrypoint failed: {err}");
                    eprintln!("fervor-init: {message}");
                    return ExitReport::InitFailed { message };
                }
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl State {
    fn signal(&self, signal: Signal) {
        let Some(pidfd) = &self.entrypoint else {
            return;
        };
        match rustix::process::pidfd_send_signal(pidfd, signal) {
            // ESRCH: the entrypoint has already exited.
            Ok(()) | Err(Errno::SRCH) => {}
            Err(err) => {
                eprintln!("fervor-init: sending {signal:?} to the entrypoint failed: {err}")
            }
        }
    }
}

impl SearchPath<'_> {
    /// Resolves `program` against the guest filesystem.
    fn resolve_executable(&self, program: &str) -> Option<std::path::PathBuf> {
        self.resolve(program, Self::is_executable)
    }

    fn is_executable(path: &Path) -> bool {
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
}
