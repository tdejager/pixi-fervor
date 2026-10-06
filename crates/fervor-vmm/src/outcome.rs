//! The outcome report `fervor-runner --outcome-file` writes, for callers that
//! drive the runner remotely (e.g. through `limactl shell`) and cannot rely
//! on its stdio: with a pty the guest console and stderr are merged.

use std::time::Duration;

use camino::Utf8Path;
use fervor_domain::machine::{GuestExit, RunOutcome};
use fervor_guest_abi::ExitReport;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerOutcome {
    /// `None` when the VM ended without the guest reporting.
    pub exit: Option<ExitReport>,
    pub wall_ms: u64,
}

pub(crate) fn guest_exit(report: ExitReport) -> GuestExit {
    match report {
        ExitReport::Exited { code } => GuestExit::Exited(code),
        ExitReport::Signaled { signal } => GuestExit::Signaled(signal),
        ExitReport::InitFailed { message } => GuestExit::InitFailed(message),
    }
}

impl RunnerOutcome {
    /// Writes the report atomically, so a reader never sees a partial file.
    pub fn write_to(&self, path: &Utf8Path) -> std::io::Result<()> {
        let staged = path.with_extension("partial");
        std::fs::write(&staged, serde_json::to_vec(self).expect("runner outcome serializes"))?;
        std::fs::rename(&staged, path)
    }
}

impl From<&RunOutcome> for RunnerOutcome {
    fn from(outcome: &RunOutcome) -> Self {
        let exit = match &outcome.exit {
            GuestExit::Exited(code) => Some(ExitReport::Exited { code: *code }),
            GuestExit::Signaled(signal) => Some(ExitReport::Signaled { signal: *signal }),
            GuestExit::InitFailed(message) => Some(ExitReport::InitFailed { message: message.clone() }),
            GuestExit::NoReport => None,
        };
        Self { exit, wall_ms: u64::try_from(outcome.wall_time.as_millis()).unwrap_or(u64::MAX) }
    }
}

impl From<RunnerOutcome> for RunOutcome {
    fn from(outcome: RunnerOutcome) -> Self {
        Self {
            exit: outcome.exit.map_or(GuestExit::NoReport, guest_exit),
            wall_time: Duration::from_millis(outcome.wall_ms),
        }
    }
}
