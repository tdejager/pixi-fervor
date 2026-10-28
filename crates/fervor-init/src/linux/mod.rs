//! The real PID 1. Every step either succeeds or ends in a reported failure;
//! in both cases init syncs and reboots, which makes Firecracker exit.

mod control;
mod events;
mod forward;
mod journal;
mod mounts;
mod network;
mod process;
mod supervisor;
mod system;
mod vsock;

use std::fmt;
use std::time::Duration;

use fervor_guest_abi::{ExitReport, GuestConfig, GuestToHost, PACK_DEVICE, RUN_DEVICE, RunConfig};

use self::control::Control;
use self::events::Wakeups;
use self::forward::ForwardListener;
use self::mounts::{EarlyMounts, Pack, RootAssembler};
use self::network::{Ethernet, Loopback};
use self::supervisor::Supervisor;
use self::system::Hostname;

/// PID 1: boots the guest, supervises the entrypoint and its services and
/// reports the entrypoint's fate to the host.
pub struct Init {
    control: Option<Control>,
}

impl Init {
    pub fn run() -> ! {
        // First: every thread init starts must inherit the blocked SIGCHLD.
        let wakeups = Wakeups::new();
        let early_mounts = EarlyMounts::mount();
        let init = Self {
            control: Control::connect(),
        };
        init.supervise(early_mounts.and(wakeups));
        Self::reboot()
    }

    /// Boots, runs the entrypoint to completion and reports the outcome.
    fn supervise(&self, wakeups: Result<Wakeups, Error>) {
        let report = match wakeups.and_then(|wakeups| self.start(wakeups)) {
            Ok(supervisor) => {
                self.send(&GuestToHost::Ready);
                supervisor.run()
            }
            Err(err) => {
                eprintln!("fervor-init: {err}");
                ExitReport::InitFailed {
                    message: err.to_string(),
                }
            }
        };
        self.send(&GuestToHost::Exited { report });
    }

    /// Assembles the root filesystem, configures the network and starts the
    /// journal, the services and the entrypoint inside it.
    fn start(&self, wakeups: Wakeups) -> Result<Supervisor, Error> {
        if let Some(control) = &self.control {
            control.spawn_reader(wakeups.shutdown());
        }
        Hostname::from_boot_layer()?.apply()?;
        Loopback::up()?;

        let image = Pack::<GuestConfig>::open(PACK_DEVICE)?;
        let run = Pack::<RunConfig>::open(RUN_DEVICE)?;
        let root = RootAssembler {
            image: &image,
            run: &run,
        }
        .assemble()?;
        let (guest, run) = (image.into_config(), run.into_config());
        root.switch()?;

        if let Some(network) = &run.network {
            Ethernet::configure(network)?;
        }
        ForwardListener::bind()?.spawn();
        Supervisor::start(wakeups, guest)
    }

    fn send(&self, message: &GuestToHost) {
        if let Some(control) = &self.control {
            control.send(message);
        }
    }

    /// Syncs and reboots, which makes Firecracker exit.
    fn reboot() -> ! {
        rustix::fs::sync();
        if let Err(err) = rustix::system::reboot(rustix::system::RebootCommand::Restart) {
            eprintln!("fervor-init: reboot failed: {err}");
        }
        // PID 1 must never exit (the kernel would panic); wait for the reset.
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
}

/// A boot step that failed, with what init was doing at the time.
#[derive(Debug)]
pub struct Error {
    context: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    pub fn msg(context: impl Into<String>) -> Self {
        Self {
            context: context.into(),
            source: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(source) => write!(f, "{}: {source}", self.context),
            None => f.write_str(&self.context),
        }
    }
}

pub trait Context<T> {
    fn context(self, context: impl FnOnce() -> String) -> Result<T, Error>;
}

impl<T, E> Context<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context(self, context: impl FnOnce() -> String) -> Result<T, Error> {
        self.map_err(|source| Error {
            context: context(),
            source: Some(Box::new(source)),
        })
    }
}
