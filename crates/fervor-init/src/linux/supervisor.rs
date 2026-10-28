//! Process supervision, the PID 1 duty: journald first, then the services in
//! order, then the entrypoint. Services restart whenever they exit; once the
//! entrypoint is gone or the host asks for a shutdown, everything stops,
//! journald last. journald is not restarted: the running processes' output
//! streams die with it and cannot be reconnected, so its exit stops the
//! guest. Spawning and reaping (orphans included) all happen on the main
//! thread, so a restart never races with `wait(2)`.

use std::time::{Duration, Instant};

use fervor_guest_abi::{ExitReport, GuestConfig, JournalConfig, ServiceConfig};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, WaitStatus};

use super::Error;
use super::events::Wakeups;
use super::journal;
use super::process::{self, Environment, Launch, Output, Process};
use crate::plan::{Alive, RestartBackoff, StopAction, StopSequence, program_identifier};

const JOURNAL_SERVICE: &str = "systemd-journald";

pub struct Supervisor {
    wakeups: Wakeups,
    environment: Environment,
    journal: Option<Service>,
    services: Vec<Service>,
    /// `None` once reaped; its fate is then in `report`.
    entrypoint: Option<Process>,
    report: Option<ExitReport>,
    /// Why the guest stops without the entrypoint deciding; overrides `report`.
    failure: Option<String>,
    stop: Option<StopSequence>,
}

impl Supervisor {
    /// Starts journald (when the image has a journal) and waits until it
    /// accepts output, then the services in order, then the entrypoint.
    pub fn start(wakeups: Wakeups, config: GuestConfig) -> Result<Self, Error> {
        let GuestConfig {
            argv,
            env,
            workdir,
            services,
            journal,
            ..
        } = config;
        let environment = Environment { env, workdir };

        let journal = match journal {
            Some(JournalConfig { daemon }) => {
                journal::prepare()?;
                let launch = Launch::resolve(
                    format!("service {JOURNAL_SERVICE}"),
                    &[daemon],
                    &environment,
                    false,
                    Output::Console,
                )?;
                let service = Service::start(JOURNAL_SERVICE.to_owned(), launch, &environment)?;
                if let Some(process) = &service.process {
                    journal::wait_until_ready(process.pid())?;
                }
                Some(service)
            }
            None => None,
        };
        let output = |identifier: &str| match journal {
            Some(_) => Output::Journal {
                identifier: identifier.to_owned(),
            },
            None => Output::Console,
        };

        let services = services
            .into_iter()
            .map(|ServiceConfig { name, argv }| {
                let launch = Launch::resolve(
                    format!("service {name}"),
                    &argv,
                    &environment,
                    false,
                    output(&name),
                )?;
                Service::start(name, launch, &environment)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let identifier = argv.first().map_or("", |argv0| program_identifier(argv0));
        let entrypoint = Launch::resolve(
            "the entrypoint".to_owned(),
            &argv,
            &environment,
            true,
            output(identifier),
        )?
        .spawn(&environment)?;

        Ok(Self {
            wakeups,
            environment,
            journal,
            services,
            entrypoint: Some(entrypoint),
            report: None,
            failure: None,
            stop: None,
        })
    }

    /// Supervises until the entrypoint and everything else has stopped and
    /// returns the entrypoint's fate.
    pub fn run(mut self) -> ExitReport {
        loop {
            if let Err(message) = self.reap() {
                eprintln!("fervor-init: {message}");
                return ExitReport::InitFailed { message };
            }
            let now = Instant::now();
            if let Some(grace) = self.wakeups.take_shutdown() {
                self.begin_stop(now, grace);
            }
            if self.advance(now) {
                if let Some(message) = self.failure.take() {
                    return ExitReport::InitFailed { message };
                }
                return self
                    .report
                    .take()
                    .unwrap_or_else(|| ExitReport::InitFailed {
                        message: "the entrypoint was never reaped".to_owned(),
                    });
            }
            self.wakeups.wait(self.next_deadline());
        }
    }

    /// Reaps every child that has terminated, known or orphaned.
    fn reap(&mut self) -> Result<(), String> {
        loop {
            match rustix::process::wait(WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => self.exited(pid, status),
                Ok(None) | Err(Errno::CHILD) => return Ok(()),
                Err(Errno::INTR) => {}
                Err(err) => return Err(format!("waiting for children failed: {err}")),
            }
        }
    }

    fn exited(&mut self, pid: Pid, status: WaitStatus) {
        let now = Instant::now();
        if self.entrypoint.as_ref().is_some_and(|p| p.pid() == pid) {
            self.entrypoint = None;
            self.report = Some(process::report(status));
            // During a host-requested stop the services keep the host's grace.
            if self.stop.is_none() {
                self.begin_stop(now, StopSequence::NATURAL_EXIT_GRACE);
            }
            return;
        }
        let stopping = self.stop.is_some();
        if let Some(journal) = self
            .journal
            .as_mut()
            .filter(|journal| journal.process.as_ref().is_some_and(|p| p.pid() == pid))
        {
            journal.process = None;
            if !stopping {
                let message = format!(
                    "{JOURNAL_SERVICE} exited ({}); the output streams of running processes \
                     cannot be reconnected, stopping the guest",
                    process::describe(status)
                );
                eprintln!("fervor-init: {message}");
                self.failure = Some(message);
                self.begin_stop(now, StopSequence::NATURAL_EXIT_GRACE);
            }
            return;
        }
        let service = self
            .services
            .iter_mut()
            .find(|service| service.process.as_ref().is_some_and(|p| p.pid() == pid));
        if let Some(service) = service {
            service.process = None;
            if !stopping {
                let uptime = now.saturating_duration_since(service.started);
                let why = format!("exited ({})", process::describe(status));
                service.schedule_restart(now, uptime, &why);
            }
        }
    }

    /// SIGTERM to the entrypoint and the services; journald keeps running
    /// until they are gone.
    fn begin_stop(&mut self, now: Instant, grace: Duration) {
        if let Some(stop) = &mut self.stop {
            stop.shorten(now, grace);
            return;
        }
        for service in self.journal.iter_mut().chain(&mut self.services) {
            service.restart_at = None;
        }
        self.signal_workload(Signal::TERM);
        self.stop = Some(StopSequence::new(now, grace));
    }

    /// Restarts services that are due, or advances the stop. Returns whether
    /// everything has stopped.
    fn advance(&mut self, now: Instant) -> bool {
        let Some(stop) = &mut self.stop else {
            for service in self.journal.iter_mut().chain(&mut self.services) {
                if service.restart_at.is_some_and(|at| at <= now) {
                    service.restart(now, &self.environment);
                }
            }
            return false;
        };
        let alive = Alive {
            workload: self.entrypoint.is_some()
                || self
                    .services
                    .iter()
                    .any(|service| service.process.is_some()),
            journal: self
                .journal
                .as_ref()
                .is_some_and(|journal| journal.process.is_some()),
        };
        let action = stop.step(now, alive);
        let done = stop.is_done();
        match action {
            Some(StopAction::KillWorkload) => self.signal_workload(Signal::KILL),
            Some(StopAction::TermJournal) => self.signal_journal(Signal::TERM),
            Some(StopAction::KillJournal) => self.signal_journal(Signal::KILL),
            None => {}
        }
        done
    }

    fn signal_workload(&self, signal: Signal) {
        if let Some(entrypoint) = &self.entrypoint {
            entrypoint.signal(signal, "the entrypoint");
        }
        for service in &self.services {
            service.signal(signal);
        }
    }

    fn signal_journal(&self, signal: Signal) {
        if let Some(journal) = &self.journal {
            journal.signal(signal);
        }
    }

    /// When the loop has to wake up even if no child exits.
    fn next_deadline(&self) -> Option<Instant> {
        match &self.stop {
            Some(stop) => stop.deadline(),
            None => self
                .journal
                .iter()
                .chain(&self.services)
                .filter_map(|service| service.restart_at)
                .min(),
        }
    }
}

/// A supervised background process, restarted whenever it exits.
struct Service {
    name: String,
    launch: Launch,
    /// `None` while waiting for a restart and once stopped.
    process: Option<Process>,
    started: Instant,
    backoff: RestartBackoff,
    restart_at: Option<Instant>,
}

impl Service {
    fn start(name: String, launch: Launch, environment: &Environment) -> Result<Self, Error> {
        let process = launch.spawn(environment)?;
        Ok(Self {
            name,
            launch,
            process: Some(process),
            started: Instant::now(),
            backoff: RestartBackoff::new(),
            restart_at: None,
        })
    }

    fn schedule_restart(&mut self, now: Instant, uptime: Duration, why: &str) {
        let delay = self.backoff.after_exit(uptime);
        eprintln!(
            "fervor-init: service {} {why}, restarting in {}s",
            self.name,
            delay.as_secs()
        );
        self.restart_at = Some(now + delay);
    }

    fn restart(&mut self, now: Instant, environment: &Environment) {
        self.restart_at = None;
        match self.launch.spawn(environment) {
            Ok(process) => {
                self.process = Some(process);
                self.started = now;
            }
            Err(err) => {
                self.schedule_restart(now, Duration::ZERO, &format!("failed to start: {err}"))
            }
        }
    }

    fn signal(&self, signal: Signal) {
        if let Some(process) = &self.process {
            process.signal(signal, self.launch.label());
        }
    }
}
