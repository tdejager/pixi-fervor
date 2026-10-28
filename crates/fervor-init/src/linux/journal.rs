//! The guest journal: `systemd-journald` runs as a supervised service and
//! the stdout/stderr of every other process is a stream connection to it.

use std::io::{self, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixStream;
use std::thread;

use fervor_guest_abi::Backoff;
use rustix::process::{Pid, WaitOptions};

use super::{Context, Error};
use crate::plan::{MachineId, StreamHeader, StreamPriority};

/// journald's stream socket for the stdout/stderr of other processes.
pub const STDOUT_SOCKET: &str = "/run/systemd/journal/stdout";
/// journald binds the syslog socket here but leaves `/dev/log` to the init
/// system.
const SYSLOG_SOCKET: &str = "/run/systemd/journal/dev-log";

/// Prepares the root filesystem for journald (after the switch into the
/// overlay root, which is writable).
pub fn prepare() -> Result<(), Error> {
    ensure_machine_id()?;
    match symlink(SYSLOG_SOCKET, "/dev/log") {
        Err(err) if err.kind() != io::ErrorKind::AlreadyExists => {
            Err(err).context(|| format!("linking /dev/log to {SYSLOG_SOCKET}"))
        }
        _ => Ok(()),
    }
}

/// Writes a random `/etc/machine-id` unless the image or run layers ship one.
fn ensure_machine_id() -> Result<(), Error> {
    const FILE: &str = "/etc/machine-id";
    match std::fs::read_to_string(FILE) {
        Ok(contents) if MachineId::is_initialized(&contents) => return Ok(()),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context(|| format!("reading {FILE}")),
    }
    let mut random = [0u8; 16];
    // SAFETY: the buffer is valid for writes of its length.
    let len = unsafe { libc::getrandom(random.as_mut_ptr().cast(), random.len(), 0) };
    if len != random.len() as isize {
        return Err(io::Error::last_os_error()).context(|| "generating a machine id".to_owned());
    }
    std::fs::create_dir_all("/etc").context(|| "creating /etc".to_owned())?;
    std::fs::write(FILE, MachineId::from_random(random).contents())
        .context(|| format!("writing {FILE}"))
}

/// Waits until the freshly spawned journald (`daemon`) accepts stream
/// connections; it is a boot failure if it exits or takes too long.
pub fn wait_until_ready(daemon: Pid) -> Result<(), Error> {
    let mut retry = Backoff::STARTUP.start();
    loop {
        let err = match UnixStream::connect(STDOUT_SOCKET) {
            Ok(_) => return Ok(()),
            Err(err) => err,
        };
        // Nothing else reaps yet: init spawns the other processes only once
        // journald is ready.
        if let Ok(Some((_, status))) = rustix::process::waitpid(Some(daemon), WaitOptions::NOHANG) {
            return Err(Error::msg(format!(
                "systemd-journald exited ({}) before accepting connections on {STDOUT_SOCKET}",
                super::process::describe(status)
            )));
        }
        match retry.next_delay() {
            Some(delay) => thread::sleep(delay),
            None => {
                return Err(err).context(|| {
                    format!(
                        "systemd-journald did not accept connections on {STDOUT_SOCKET} within {}s",
                        Backoff::STARTUP.window.as_secs()
                    )
                });
            }
        }
    }
}

/// A connection to journald that turns everything written to it into
/// journal entries of `identifier`.
pub fn stream(identifier: &str, priority: StreamPriority) -> Result<OwnedFd, Error> {
    let output = match priority {
        StreamPriority::Stdout => "stdout",
        StreamPriority::Stderr => "stderr",
    };
    let context = || format!("connecting the {output} of {identifier} to {STDOUT_SOCKET}");
    let mut stream = UnixStream::connect(STDOUT_SOCKET).context(context)?;
    stream
        .write_all(
            &StreamHeader {
                identifier,
                priority,
            }
            .encode(),
        )
        .context(context)?;
    Ok(stream.into())
}
