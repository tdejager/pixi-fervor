//! What wakes the supervisor loop: children changing state (SIGCHLD, read
//! through a signalfd) and shutdown requests from the host (an eventfd).

use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec};
use rustix::io::Errno;

use super::{Context, Error};

pub struct Wakeups {
    sigchld: OwnedFd,
    shutdown: Arc<ShutdownRequest>,
}

impl Wakeups {
    /// Blocks SIGCHLD so that it only arrives through the signalfd. Must run
    /// before init starts any thread: threads inherit the signal mask, and
    /// one with SIGCHLD unblocked would swallow it. Children clear the mask
    /// before exec (`Launch::spawn`).
    pub fn new() -> Result<Self, Error> {
        let context = || "setting up the SIGCHLD signalfd".to_owned();
        // SAFETY: `sigset_t` is plain old data, initialized by sigemptyset.
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: `mask` is a valid sigset_t that outlives the calls.
        let fd = unsafe {
            libc::sigemptyset(&mut mask);
            libc::sigaddset(&mut mask, libc::SIGCHLD);
            let err = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            if err != 0 {
                return Err(io::Error::from_raw_os_error(err)).context(context);
            }
            libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error()).context(context);
        }
        // SAFETY: signalfd returned a new descriptor that nothing else owns.
        let sigchld = unsafe { OwnedFd::from_raw_fd(fd) };
        let wake = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
            .context(|| "creating the shutdown eventfd".to_owned())?;
        Ok(Self {
            sigchld,
            shutdown: Arc::new(ShutdownRequest {
                grace: Mutex::new(None),
                wake,
            }),
        })
    }

    /// Where the control channel delivers the host's shutdown requests.
    pub fn shutdown(&self) -> Arc<ShutdownRequest> {
        Arc::clone(&self.shutdown)
    }

    /// The grace of a shutdown requested since the last call, if any.
    pub fn take_shutdown(&self) -> Option<Duration> {
        self.shutdown
            .grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Blocks until a child may have changed state, a shutdown was requested
    /// or `deadline` has passed. Spurious returns are fine: the caller
    /// re-checks everything.
    pub fn wait(&self, deadline: Option<Instant>) {
        let timeout = deadline.map(|deadline| {
            let left = deadline.saturating_duration_since(Instant::now());
            Timespec::try_from(left).unwrap_or(Timespec {
                tv_sec: i64::MAX,
                tv_nsec: 0,
            })
        });
        let mut fds = [
            PollFd::new(&self.sigchld, PollFlags::IN),
            PollFd::new(&self.shutdown.wake, PollFlags::IN),
        ];
        match rustix::event::poll(&mut fds, timeout.as_ref()) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(err) => {
                eprintln!("fervor-init: polling for child and host events failed: {err}");
                // Keeps a persistent failure from spinning.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        // Drained before the caller looks, so nothing arriving afterwards is
        // lost: it makes the descriptor readable again.
        Self::drain(&self.sigchld, size_of::<libc::signalfd_siginfo>());
        Self::drain(&self.shutdown.wake, size_of::<u64>());
    }

    fn drain(fd: impl AsFd, record_len: usize) {
        let mut buf = [0u8; 1024];
        let records = buf.len() / record_len * record_len;
        while let Ok(1..) = rustix::io::read(&fd, &mut buf[..records]) {}
    }
}

/// A shutdown requested by the host, handed from the control thread to the
/// supervisor loop.
pub struct ShutdownRequest {
    grace: Mutex<Option<Duration>>,
    wake: OwnedFd,
}

impl ShutdownRequest {
    /// SIGTERM to the entrypoint and services, SIGKILL after `grace`.
    pub fn request(&self, grace: Duration) {
        let mut pending = self
            .grace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *pending = Some(pending.map_or(grace, |earlier| earlier.min(grace)));
        drop(pending);
        if let Err(err) = rustix::io::write(&self.wake, &1u64.to_ne_bytes()) {
            eprintln!("fervor-init: waking the supervisor for a shutdown failed: {err}");
        }
    }
}
