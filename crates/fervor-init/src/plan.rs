//! Pure decisions of the boot sequence, kept apart from the syscalls so they
//! can be tested on any host.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fervor_guest_abi::pack::PACK_ALIGN;
use fervor_guest_abi::{NetworkConfig, PackEntry};

/// The entrypoint's own `PATH` (not init's), used to resolve `argv[0]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchPath<'a> {
    var: Option<&'a str>,
}

impl<'a> SearchPath<'a> {
    pub fn new(var: Option<&'a str>) -> Self {
        Self { var }
    }

    /// The `PATH` entry of an environment, if any.
    pub fn from_env(env: &'a [(String, String)]) -> Self {
        Self::new(
            env.iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.as_str()),
        )
    }

    /// Resolves `program` the way `execvp` would: a name containing `/` is
    /// used as is, otherwise the first executable `<dir>/<name>` along the
    /// path wins. Empty entries are skipped instead of meaning "current
    /// directory".
    pub fn resolve(&self, program: &str, is_executable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
        if program.is_empty() {
            return None;
        }
        if program.contains('/') {
            return Some(PathBuf::from(program));
        }
        self.var?
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join(program))
            .find(|candidate| is_executable(candidate))
    }
}

/// The raw `PATH` value, or `unset`.
impl fmt::Display for SearchPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.var.unwrap_or("unset"))
    }
}

/// Overlay lower directories, uppermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLowers(Vec<PathBuf>);

impl OverlayLowers {
    /// The run pack's layers above the image pack's (each given bottom →
    /// top, as in the pack headers), uppermost first, then the boot root at
    /// the bottom.
    pub fn new(image_layers: &[PathBuf], run_layers: &[PathBuf], boot_root: &Path) -> Self {
        Self(
            image_layers
                .iter()
                .chain(run_layers)
                .rev()
                .cloned()
                .chain(std::iter::once(boot_root.to_path_buf()))
                .collect(),
        )
    }

    pub fn as_slice(&self) -> &[PathBuf] {
        &self.0
    }
}

/// The byte range of the pack a layer's loop device exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopRange {
    pub offset: u64,
    /// The loop driver drops a trailing partial 512-byte sector, which would
    /// cut off the end of an image whose length is not sector aligned; every
    /// layer owns its slot up to the next `PACK_ALIGN` boundary (zero
    /// padding), so the range ends at the slot end instead.
    pub size_limit: u64,
}

impl LoopRange {
    pub fn of(entry: &PackEntry) -> Self {
        Self {
            offset: entry.offset,
            size_limit: entry.len.div_ceil(PACK_ALIGN) * PACK_ALIGN,
        }
    }
}

/// The IPv4 settings of `eth0`, checked before any of them is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Setup {
    pub address: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
}

impl Ipv4Setup {
    pub fn of(config: &NetworkConfig) -> Result<Self, String> {
        let NetworkConfig {
            address,
            prefix_len,
            gateway,
            ..
        } = *config;
        if prefix_len > 32 {
            return Err(format!(
                "prefix length /{prefix_len} is longer than 32 bits"
            ));
        }
        // A shift by 32 overflows, so /0 needs its own case.
        let mask = u32::MAX
            .checked_shl(32 - u32::from(prefix_len))
            .unwrap_or(0);
        let network = |ip: Ipv4Addr| u32::from(ip) & mask;
        if gateway == address || network(gateway) != network(address) {
            return Err(format!(
                "gateway {gateway} is not another host of {address}/{prefix_len}"
            ));
        }
        Ok(Self {
            address,
            netmask: Ipv4Addr::from(mask),
            gateway,
        })
    }
}

/// Which output of a process a journal stream carries, as its syslog
/// priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPriority {
    /// `LOG_INFO`
    Stdout = 6,
    /// `LOG_ERR`
    Stderr = 3,
}

/// The setup header journald expects on a fresh connection to
/// `/run/systemd/journal/stdout` (`stdout_stream_line()` in systemd's
/// `journald-stream.c`): seven newline-terminated lines, then log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHeader<'a> {
    pub identifier: &'a str,
    pub priority: StreamPriority,
}

impl StreamHeader<'_> {
    /// journald rejects a setup line whose newline is not within its first
    /// 254 bytes (`UNIT_NAME_MAX - 1`), which ends the stream.
    const LINE_MAX: usize = 254;

    pub fn encode(&self) -> Vec<u8> {
        let identifier = self.sanitized_identifier();
        let mut header = Vec::with_capacity(identifier.len() + 16);
        for line in [
            identifier.as_str(),
            // Unit id: journald only honours it from root and for valid unit
            // names; output is attributed by identifier instead.
            "",
            &(self.priority as u8).to_string(),
            // level_prefix: `<3>`-style prefixes override the priority.
            "1",
            // forward_to_syslog, forward_to_kmsg
            "0",
            "0",
            // forward_to_console: output still reaches the serial console.
            "1",
        ] {
            header.extend_from_slice(line.as_bytes());
            header.push(b'\n');
        }
        header
    }

    /// The identifier as a single setup line: line breaks and NULs would
    /// end it early, and it must fit in `LINE_MAX` with its newline.
    fn sanitized_identifier(&self) -> String {
        let mut identifier: String = self
            .identifier
            .chars()
            .map(|c| if matches!(c, '\n' | '\0') { '_' } else { c })
            .collect();
        let mut end = identifier.len().min(Self::LINE_MAX - 1);
        while !identifier.is_char_boundary(end) {
            end -= 1;
        }
        identifier.truncate(end);
        identifier
    }
}

/// The journal identifier of a program: the basename of its `argv[0]`, like
/// systemd's `SyslogIdentifier=` default.
pub fn program_identifier(argv0: &str) -> &str {
    match argv0.rsplit('/').next() {
        Some(name) if !name.is_empty() => name,
        _ => argv0,
    }
}

/// The contents of `/etc/machine-id`, which journald needs to name its
/// journal directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineId(String);

impl MachineId {
    /// A random v4 UUID in machine-id format, as `sd_id128_randomize()`
    /// makes it.
    pub fn from_random(mut bytes: [u8; 16]) -> Self {
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let mut id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        id.push('\n');
        Self(id)
    }

    /// Whether an existing `/etc/machine-id` already holds an id; an empty
    /// file is the conventional "generate one at boot" marker.
    pub fn is_initialized(contents: &str) -> bool {
        let id = contents.strip_suffix('\n').unwrap_or(contents);
        id.len() == 32
            && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && id.bytes().any(|b| b != b'0')
    }

    pub fn contents(&self) -> &str {
        &self.0
    }
}

/// Increasing delays between restarts of a service that keeps exiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartBackoff {
    next: Duration,
}

impl RestartBackoff {
    pub const INITIAL: Duration = Duration::from_secs(1);
    pub const MAX_DELAY: Duration = Duration::from_secs(30);
    /// A service that ran at least this long before exiting starts over at
    /// `INITIAL`, so a rare crash is not punished by an old crash loop.
    pub const STABLE_UPTIME: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }

    /// The delay before restarting a service that exited after running for
    /// `uptime`.
    pub fn after_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE_UPTIME {
            self.next = Self::INITIAL;
        }
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX_DELAY);
        delay
    }
}

/// What is still running while the guest stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Alive {
    /// The entrypoint or any service.
    pub workload: bool,
    pub journal: bool,
}

/// A signal the stop sequence asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopAction {
    /// SIGKILL the entrypoint and every service: their grace has run out.
    KillWorkload,
    /// SIGTERM the journal daemon: everything that logs to it is gone.
    TermJournal,
    KillJournal,
}

/// Stopping the guest: the entrypoint and services have been sent SIGTERM;
/// they get SIGKILL once their grace runs out, and the journal daemon is
/// stopped only after all of them are gone, so their last output is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopSequence {
    stage: StopStage,
    /// When the current stage's SIGKILL is due.
    deadline: Instant,
    killed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopStage {
    Workload,
    Journal,
    Done,
}

impl StopSequence {
    /// Grace after the entrypoint exited on its own (the host asks for its
    /// own grace when it requests the shutdown).
    pub const NATURAL_EXIT_GRACE: Duration = Duration::from_secs(5);
    /// journald syncs and exits right away on SIGTERM.
    pub const JOURNAL_GRACE: Duration = Duration::from_secs(2);

    /// Starts once SIGTERM went to the entrypoint and the services.
    pub fn new(now: Instant, grace: Duration) -> Self {
        Self {
            stage: StopStage::Workload,
            deadline: now + grace,
            killed: false,
        }
    }

    /// A second stop request can only bring the workload's SIGKILL forward.
    pub fn shorten(&mut self, now: Instant, grace: Duration) {
        if self.stage == StopStage::Workload && !self.killed {
            self.deadline = self.deadline.min(now + grace);
        }
    }

    /// Advances the sequence given what is still running at `now`.
    pub fn step(&mut self, now: Instant, alive: Alive) -> Option<StopAction> {
        match self.stage {
            StopStage::Workload if !alive.workload => {
                if !alive.journal {
                    self.stage = StopStage::Done;
                    return None;
                }
                *self = Self {
                    stage: StopStage::Journal,
                    deadline: now + Self::JOURNAL_GRACE,
                    killed: false,
                };
                Some(StopAction::TermJournal)
            }
            StopStage::Journal if !alive.journal => {
                self.stage = StopStage::Done;
                None
            }
            StopStage::Workload | StopStage::Journal if !self.killed && now >= self.deadline => {
                self.killed = true;
                Some(match self.stage {
                    StopStage::Workload => StopAction::KillWorkload,
                    _ => StopAction::KillJournal,
                })
            }
            _ => None,
        }
    }

    /// When `step` has to run again even if nothing exits.
    pub fn deadline(&self) -> Option<Instant> {
        (self.stage != StopStage::Done && !self.killed).then_some(self.deadline)
    }

    pub fn is_done(&self) -> bool {
        self.stage == StopStage::Done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bare_names_along_path_in_order() {
        let executables = ["/opt/env/bin/python", "/usr/bin/python"];
        let is_exec = |p: &Path| executables.iter().any(|e| Path::new(e) == p);
        let path = |var| SearchPath::new(Some(var));
        assert_eq!(
            path(":/missing:/opt/env/bin:/usr/bin").resolve("python", is_exec),
            Some(PathBuf::from("/opt/env/bin/python"))
        );
        assert_eq!(
            path("/usr/bin:/opt/env/bin").resolve("python", is_exec),
            Some(PathBuf::from("/usr/bin/python"))
        );
        assert_eq!(SearchPath::new(None).resolve("python", is_exec), None);
        assert_eq!(path("/opt/env/bin").resolve("flask", is_exec), None);
        assert_eq!(path("/opt/env/bin").resolve("", |_| true), None);
    }

    #[test]
    fn names_with_a_slash_bypass_path() {
        assert_eq!(
            SearchPath::new(Some("/bin")).resolve("./run.sh", |_| false),
            Some(PathBuf::from("./run.sh"))
        );
        assert_eq!(
            SearchPath::new(None).resolve("/bin/sh", |_| false),
            Some(PathBuf::from("/bin/sh"))
        );
    }

    fn paths(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn overlay_lowers_put_run_layers_above_image_layers_and_boot_last() {
        let image = paths(&["/fervor/l/0", "/fervor/l/1"]);
        let run = paths(&["/fervor/r/0", "/fervor/r/1"]);
        let boot = Path::new("/fervor/boot");
        assert_eq!(
            OverlayLowers::new(&image, &run, boot).as_slice(),
            paths(&[
                "/fervor/r/1",
                "/fervor/r/0",
                "/fervor/l/1",
                "/fervor/l/0",
                "/fervor/boot"
            ])
        );
        assert_eq!(
            OverlayLowers::new(&image, &[], boot).as_slice(),
            paths(&["/fervor/l/1", "/fervor/l/0", "/fervor/boot"])
        );
        assert_eq!(
            OverlayLowers::new(&[], &[], boot).as_slice(),
            paths(&["/fervor/boot"])
        );
    }

    #[test]
    fn loop_range_covers_the_whole_aligned_slot() {
        let limit = |len| {
            LoopRange::of(&PackEntry {
                digest: [0; 32],
                offset: PACK_ALIGN,
                len,
            })
            .size_limit
        };
        assert_eq!(limit(1), PACK_ALIGN);
        assert_eq!(limit(PACK_ALIGN), PACK_ALIGN);
        assert_eq!(limit(PACK_ALIGN + 1), 2 * PACK_ALIGN);
    }

    fn network(address: [u8; 4], prefix_len: u8, gateway: [u8; 4]) -> NetworkConfig {
        NetworkConfig {
            address: Ipv4Addr::from(address),
            prefix_len,
            gateway: Ipv4Addr::from(gateway),
            resolv_conf: String::new(),
        }
    }

    #[test]
    fn netmask_follows_the_prefix_length_at_both_extremes() {
        let netmask = |prefix_len, gateway| {
            Ipv4Setup::of(&network([172, 29, 0, 2], prefix_len, gateway)).map(|s| s.netmask)
        };
        assert_eq!(
            netmask(30, [172, 29, 0, 1]),
            Ok(Ipv4Addr::new(255, 255, 255, 252))
        );
        assert_eq!(
            netmask(16, [172, 29, 9, 9]),
            Ok(Ipv4Addr::new(255, 255, 0, 0))
        );
        assert_eq!(netmask(0, [10, 0, 0, 1]), Ok(Ipv4Addr::UNSPECIFIED));
        assert!(netmask(33, [172, 29, 0, 1]).is_err());
    }

    #[test]
    fn the_gateway_must_be_another_host_on_the_link() {
        let setup =
            |address, prefix_len, gateway| Ipv4Setup::of(&network(address, prefix_len, gateway));
        // 172.29.0.5 is in the next /30 slot: the route would be unreachable.
        assert!(setup([172, 29, 0, 2], 30, [172, 29, 0, 5]).is_err());
        assert!(setup([172, 29, 0, 2], 30, [172, 29, 0, 2]).is_err());
        assert!(setup([172, 29, 0, 2], 32, [172, 29, 0, 1]).is_err());
        assert_eq!(
            setup([172, 29, 0, 6], 30, [172, 29, 0, 5]).map(|s| s.gateway),
            Ok(Ipv4Addr::new(172, 29, 0, 5))
        );
    }

    #[test]
    fn stream_header_has_journalds_seven_setup_lines() {
        let header = |identifier, priority| {
            String::from_utf8(
                StreamHeader {
                    identifier,
                    priority,
                }
                .encode(),
            )
            .unwrap()
        };
        assert_eq!(
            header("web", StreamPriority::Stdout),
            "web\n\n6\n1\n0\n0\n1\n"
        );
        assert_eq!(
            header("web", StreamPriority::Stderr),
            "web\n\n3\n1\n0\n0\n1\n"
        );
    }

    #[test]
    fn stream_header_identifier_stays_one_bounded_line() {
        fn encode(identifier: &str) -> Vec<u8> {
            StreamHeader {
                identifier,
                priority: StreamPriority::Stdout,
            }
            .encode()
        }
        assert_eq!(encode("a\nb\0c"), b"a_b_c\n\n6\n1\n0\n0\n1\n");
        let first_line = |header: &[u8]| header.iter().position(|&b| b == b'\n').unwrap() + 1;
        assert_eq!(
            first_line(&encode(&"x".repeat(1000))),
            StreamHeader::LINE_MAX
        );
        // Truncation never splits a multi-byte character.
        let header = encode(&"é".repeat(200));
        assert!(first_line(&header) <= StreamHeader::LINE_MAX);
        assert!(String::from_utf8(header).is_ok());
    }

    #[test]
    fn program_identifier_is_the_basename() {
        assert_eq!(program_identifier("/opt/env/bin/python"), "python");
        assert_eq!(program_identifier("python"), "python");
        assert_eq!(program_identifier("./run.sh"), "run.sh");
        assert_eq!(program_identifier("odd/"), "odd/");
    }

    #[test]
    fn machine_id_is_a_v4_uuid_in_lowercase_hex() {
        let id = MachineId::from_random([0xff; 16]);
        assert_eq!(id.contents(), "ffffffffffff4fffbfffffffffffffff\n");
        assert!(MachineId::is_initialized(id.contents()));
        assert!(MachineId::is_initialized(
            MachineId::from_random([0; 16]).contents()
        ));
    }

    #[test]
    fn machine_id_placeholders_are_not_initialized() {
        assert!(!MachineId::is_initialized(""));
        assert!(!MachineId::is_initialized("uninitialized\n"));
        assert!(!MachineId::is_initialized(&format!("{}\n", "0".repeat(32))));
        assert!(!MachineId::is_initialized(&format!("{}\n", "A".repeat(32))));
        assert!(MachineId::is_initialized(&"a".repeat(32)));
    }

    #[test]
    fn restart_delays_double_up_to_the_cap() {
        let mut backoff = RestartBackoff::new();
        let delays: Vec<u64> = (0..8)
            .map(|_| backoff.after_exit(Duration::ZERO).as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn a_stable_run_resets_the_restart_delay() {
        let mut backoff = RestartBackoff::new();
        for _ in 0..5 {
            backoff.after_exit(Duration::ZERO);
        }
        assert_eq!(
            backoff.after_exit(RestartBackoff::STABLE_UPTIME),
            RestartBackoff::INITIAL
        );
        assert_eq!(
            backoff.after_exit(Duration::ZERO),
            2 * RestartBackoff::INITIAL
        );
    }

    const BOTH: Alive = Alive {
        workload: true,
        journal: true,
    };
    const JOURNAL_ONLY: Alive = Alive {
        workload: false,
        journal: true,
    };
    const NOTHING: Alive = Alive {
        workload: false,
        journal: false,
    };

    #[test]
    fn workload_is_killed_once_after_its_grace() {
        let start = Instant::now();
        let grace = Duration::from_secs(3);
        let mut stop = StopSequence::new(start, grace);
        assert_eq!(stop.deadline(), Some(start + grace));
        assert_eq!(stop.step(start + Duration::from_secs(1), BOTH), None);
        assert_eq!(
            stop.step(start + grace, BOTH),
            Some(StopAction::KillWorkload)
        );
        assert_eq!(stop.step(start + 2 * grace, BOTH), None);
        assert_eq!(stop.deadline(), None, "nothing left to time after SIGKILL");
        assert!(!stop.is_done());
    }

    #[test]
    fn journal_stops_only_after_the_workload_and_has_its_own_grace() {
        let start = Instant::now();
        let mut stop = StopSequence::new(start, Duration::from_secs(10));
        let gone = start + Duration::from_secs(1);
        assert_eq!(stop.step(gone, JOURNAL_ONLY), Some(StopAction::TermJournal));
        assert_eq!(stop.deadline(), Some(gone + StopSequence::JOURNAL_GRACE));
        assert_eq!(stop.step(gone, JOURNAL_ONLY), None);
        assert_eq!(
            stop.step(gone + StopSequence::JOURNAL_GRACE, JOURNAL_ONLY),
            Some(StopAction::KillJournal)
        );
        assert_eq!(stop.step(gone + StopSequence::JOURNAL_GRACE, NOTHING), None);
        assert!(stop.is_done());
    }

    #[test]
    fn without_a_journal_the_stop_ends_with_the_workload() {
        let start = Instant::now();
        let mut stop = StopSequence::new(start, Duration::from_secs(5));
        assert_eq!(stop.step(start, NOTHING), None);
        assert!(stop.is_done());
        assert_eq!(stop.deadline(), None);
    }

    #[test]
    fn a_later_stop_request_only_brings_the_kill_forward() {
        let start = Instant::now();
        let mut stop = StopSequence::new(start, Duration::from_secs(5));
        stop.shorten(start, Duration::from_secs(30));
        assert_eq!(stop.deadline(), Some(start + Duration::from_secs(5)));
        stop.shorten(start, Duration::from_secs(1));
        assert_eq!(stop.deadline(), Some(start + Duration::from_secs(1)));
    }
}
