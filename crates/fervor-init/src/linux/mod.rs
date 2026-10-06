//! The real PID 1. Every step either succeeds or ends in a reported failure;
//! in both cases init syncs and reboots, which makes Firecracker exit.

mod control;
mod entrypoint;
mod forward;
mod mounts;
mod system;
mod vsock;

use std::fmt;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;

use fervor_guest_abi::{ExitReport, GuestToHost, PACK_DEVICE, PackHeader};
use rustix::process::Pid;

use self::control::Control;
use self::entrypoint::Supervisor;
use self::forward::ForwardListener;
use self::mounts::{EarlyMounts, RootAssembler};
use self::system::{Hostname, Loopback};

/// PID 1: boots the guest, supervises the entrypoint and reports its fate to
/// the host.
pub struct Init {
    control: Option<Control>,
    supervisor: Arc<Supervisor>,
}

impl Init {
    pub fn run() -> ! {
        let early_mounts = EarlyMounts::mount();
        let init = Self::connect();
        init.supervise(early_mounts);
        Self::reboot()
    }

    /// Opens the control channel (if the host answers) and starts handling
    /// its requests.
    fn connect() -> Self {
        let init = Self {
            control: Control::connect(),
            supervisor: Arc::new(Supervisor::default()),
        };
        if let Some(control) = &init.control {
            control.spawn_reader(Arc::clone(&init.supervisor));
        }
        init
    }

    /// Boots, runs the entrypoint to completion and reports the outcome.
    fn supervise(&self, early_mounts: Result<(), Error>) {
        let report = match early_mounts.and_then(|()| self.start()) {
            Ok(pid) => {
                self.send(&GuestToHost::Ready);
                self.supervisor.wait_for_exit(pid)
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

    /// Assembles the root filesystem and spawns the entrypoint inside it.
    fn start(&self) -> Result<Pid, Error> {
        Hostname::from_boot_layer()?.apply()?;
        Loopback::up()?;

        let (pack, header) = Self::read_pack()?;
        let root = RootAssembler {
            pack: &pack,
            header: &header,
        }
        .assemble()?;
        drop(pack);
        root.switch()?;

        ForwardListener::bind()?.spawn();
        self.supervisor.spawn(header.config())
    }

    fn read_pack() -> Result<(File, PackHeader), Error> {
        let pack =
            File::open(PACK_DEVICE).context(|| format!("opening the pack device {PACK_DEVICE}"))?;
        let header = PackHeader::read_from(&pack)
            .context(|| format!("reading the pack header from {PACK_DEVICE}"))?;
        Ok((pack, header))
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
