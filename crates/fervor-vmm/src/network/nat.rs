//! The run host's side of `--network nat`: a persistent TAP device with the
//! host's address, IPv4 forwarding and `iptables` NAT/forward rules, all
//! removed again when the [`NatNetwork`] is dropped.
//!
//! A slot is claimed by binding an abstract Unix socket rather than by the
//! TAP device alone: the persistent device and the rules outlive a run that
//! is killed (SIGKILL, OOM) before it can remove them, but the socket does
//! not. Abstract names live in the network namespace, like TAP devices, and
//! the kernel releases them when the owning process dies, without leaving a
//! file behind. So a bindable slot is owned by no live run, and whatever it
//! left behind is stale and removed before the slot is set up again.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use fervor_guest_abi::NetworkConfig;

use super::{PREFIX_LEN, Rule, SLOTS, Slot};
use crate::error::RunError;
use crate::firecracker::NetworkInterface;

/// Bit of `CAP_NET_ADMIN` in the capability sets of `/proc/self/status`.
const CAP_NET_ADMIN: u32 = 12;
/// The flags Firecracker (v1.17 `Tap::open_named`) opens the TAP device with.
const TAP_FLAGS: libc::c_int = libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR;
const IP_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";

/// A claimed slot with its TAP device configured and NAT in place.
pub(crate) struct NatNetwork {
    slot: Slot,
    tap: String,
    /// Rules added so far, in order.
    rules: Vec<Rule>,
    /// The slot's claim, held until everything above is removed (fields are
    /// dropped after [`Drop::drop`]).
    _claim: UnixDatagram,
}

impl NatNetwork {
    /// Fails unless this process may set up the network: root with
    /// CAP_NET_ADMIN. The capability alone is not enough when it comes from
    /// file capabilities, since the `iptables` we run would not inherit it.
    pub(crate) fn check_permission() -> Result<(), RunError> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(RunError::NoNetAdmin);
        }
        let status = fs::read_to_string("/proc/self/status").map_err(|e| RunError::infrastructure("reading /proc/self/status", e))?;
        let effective = status
            .lines()
            .find_map(|line| line.strip_prefix("CapEff:"))
            .and_then(|caps| u64::from_str_radix(caps.trim(), 16).ok());
        match effective {
            Some(caps) if caps & (1 << CAP_NET_ADMIN) != 0 => Ok(()),
            _ => Err(RunError::NoNetAdmin),
        }
    }

    /// Claims a slot and sets up its TAP device and NAT; blocking. Whatever
    /// was set up is removed again if a later step fails.
    pub(crate) fn setup() -> Result<Self, RunError> {
        let (slot, claim, tun) = claim_slot()?;
        let tap = slot.tap_name();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        // The device must outlive our descriptor, which Firecracker needs to
        // attach to it; owning it lets Firecracker attach without privileges.
        tun_ioctl(&tun, libc::TUNSETOWNER, libc::c_ulong::from(uid))
            .and_then(|()| tun_ioctl(&tun, libc::TUNSETPERSIST, 1))
            .map_err(|e| RunError::infrastructure(format!("making {tap} persistent"), e))?;
        drop(tun);

        let mut network = Self { slot, tap, rules: Vec::new(), _claim: claim };
        network.configure_link().map_err(|e| RunError::infrastructure(format!("configuring {}", network.tap), e))?;
        enable_ip_forwarding().map_err(|e| RunError::infrastructure(format!("enabling IPv4 forwarding ({IP_FORWARD})"), e))?;
        for rule in slot.rules() {
            rule.add().map_err(|e| RunError::infrastructure(format!("adding the NAT rules for {}", network.tap), e))?;
            network.rules.push(rule);
        }
        Ok(network)
    }

    /// Firecracker's view: the guest's `eth0` on our TAP device.
    pub(crate) fn interface(&self) -> NetworkInterface {
        NetworkInterface::new(self.tap.clone(), self.slot.guest_mac())
    }

    /// The guest's view, for the run pack.
    pub(crate) fn guest_config(&self, resolv_conf: String) -> NetworkConfig {
        NetworkConfig { address: self.slot.guest(), prefix_len: PREFIX_LEN, gateway: self.slot.host(), resolv_conf }
    }

    /// Host address and netmask, link up.
    fn configure_link(&self) -> io::Result<()> {
        // SAFETY: socket has no preconditions; the result is checked.
        let socket = cvt(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
        // SAFETY: a new descriptor that nothing else owns.
        let socket = unsafe { OwnedFd::from_raw_fd(socket) };

        let mut request = ifreq(&self.tap);
        request.ifr_ifru.ifru_addr = sockaddr(self.slot.host());
        if_ioctl(&socket, libc::SIOCSIFADDR, &mut request)?;
        let mut request = ifreq(&self.tap);
        request.ifr_ifru.ifru_netmask = sockaddr(Slot::netmask());
        if_ioctl(&socket, libc::SIOCSIFNETMASK, &mut request)?;
        let mut request = ifreq(&self.tap);
        if_ioctl(&socket, libc::SIOCGIFFLAGS, &mut request)?;
        // SAFETY: SIOCGIFFLAGS filled in the flags.
        unsafe { request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
        if_ioctl(&socket, libc::SIOCSIFFLAGS, &mut request)
    }
}

impl Drop for NatNetwork {
    fn drop(&mut self) {
        for rule in self.rules.drain(..).rev() {
            if let Err(e) = rule.delete() {
                tracing::warn!("removing a NAT rule for {}: {e}", self.tap);
            }
        }
        if let Err(e) = remove_tap(&self.tap) {
            tracing::warn!("removing {}: {e}", self.tap);
        }
    }
}

/// Claims the first slot no live run holds, clears what a crashed run may
/// have left in it and creates its TAP device. `IFF_TUN_EXCL` makes creation
/// fail if the device still exists, a second guard against sharing one.
fn claim_slot() -> Result<(Slot, UnixDatagram, File), RunError> {
    for slot in (0..SLOTS).map(Slot) {
        let tap = slot.tap_name();
        let Some(claim) = bind_claim(slot).map_err(|e| RunError::infrastructure(format!("claiming {tap}"), e))? else {
            continue;
        };
        match remove_stale_tap(&tap) {
            Ok(()) => {}
            // Still attached to, by a process that outlived its run (e.g. an
            // orphaned Firecracker): leave it alone.
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                tracing::warn!("skipping {tap}: left behind by a crashed run but still in use");
                continue;
            }
            Err(e) => return Err(RunError::infrastructure(format!("removing the stale {tap}"), e)),
        }
        for rule in slot.rules() {
            rule.remove_all().map_err(|e| RunError::infrastructure(format!("removing stale NAT rules for {tap}"), e))?;
        }
        let tun = open_tun().map_err(|e| RunError::infrastructure("opening /dev/net/tun", e))?;
        match set_iff(&tun, &tap, TAP_FLAGS | libc::IFF_TUN_EXCL) {
            Ok(()) => return Ok((slot, claim, tun)),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => continue,
            Err(e) => return Err(RunError::infrastructure(format!("creating TAP device {tap}"), e)),
        }
    }
    Err(RunError::infrastructure("creating a TAP device", format!("all {SLOTS} fervor network slots are in use")))
}

/// Binds the abstract socket claiming `slot`; `None` if a live run holds it.
fn bind_claim(slot: Slot) -> io::Result<Option<UnixDatagram>> {
    let name = SocketAddr::from_abstract_name(format!("fervor/net/{}", slot.0))?;
    match UnixDatagram::bind_addr(&name) {
        Ok(socket) => Ok(Some(socket)),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(None),
        Err(e) => Err(e),
    }
}

/// Removes `name` if a crashed run left it behind: attaching creates it if
/// absent, either way it is gone once the (non-persistent) descriptor is
/// closed. Busy if something is still attached.
fn remove_stale_tap(name: &str) -> io::Result<()> {
    let tun = open_tun()?;
    set_iff(&tun, name, TAP_FLAGS)?;
    tun_ioctl(&tun, libc::TUNSETPERSIST, 0)
}

/// Attaches to the (no longer used) device and clears its persist flag, so
/// it disappears when the descriptor is closed. Firecracker may still be
/// releasing it when a run is torn down abnormally, so busy is retried.
fn remove_tap(name: &str) -> io::Result<()> {
    const ATTEMPTS: u32 = 20;
    let tun = open_tun()?;
    let mut attempt = 1;
    loop {
        match set_iff(&tun, name, TAP_FLAGS) {
            Ok(()) => break,
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) && attempt < ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
    tun_ioctl(&tun, libc::TUNSETPERSIST, 0)
}

fn enable_ip_forwarding() -> io::Result<()> {
    // Writing needs more than CAP_NET_ADMIN; skip it when already on.
    if fs::read_to_string(IP_FORWARD)?.trim() == "1" {
        return Ok(());
    }
    fs::write(IP_FORWARD, "1")
}

fn open_tun() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/net/tun")
}

fn set_iff(tun: &File, name: &str, flags: libc::c_int) -> io::Result<()> {
    let mut request = ifreq(name);
    request.ifr_ifru.ifru_flags = flags as libc::c_short;
    // SAFETY: TUNSETIFF reads and writes the ifreq, which we own.
    cvt(unsafe { libc::ioctl(tun.as_raw_fd(), libc::TUNSETIFF, &mut request) }).map(drop)
}

/// A TUN ioctl that takes its argument by value.
fn tun_ioctl(tun: &File, request: libc::Ioctl, value: libc::c_ulong) -> io::Result<()> {
    // SAFETY: TUNSETOWNER and TUNSETPERSIST take an integer, not a pointer.
    cvt(unsafe { libc::ioctl(tun.as_raw_fd(), request, value) }).map(drop)
}

/// An interface ioctl (`SIOC[GS]IF*`) on `socket`.
fn if_ioctl(socket: &OwnedFd, request: libc::c_ulong, ifreq: &mut libc::ifreq) -> io::Result<()> {
    // SAFETY: the SIOC[GS]IF* requests read or write the ifreq, which we own.
    cvt(unsafe { libc::ioctl(socket.as_raw_fd(), request as libc::Ioctl, ifreq as *mut libc::ifreq) }).map(drop)
}

fn ifreq(name: &str) -> libc::ifreq {
    // SAFETY: all-zero is a valid ifreq (empty name, zero union).
    let mut request: libc::ifreq = unsafe { mem::zeroed() };
    debug_assert!(name.len() < request.ifr_name.len(), "interface names fit IFNAMSIZ");
    for (dst, src) in request.ifr_name.iter_mut().zip(name.bytes()) {
        *dst = src as libc::c_char;
    }
    request
}

fn sockaddr(address: Ipv4Addr) -> libc::sockaddr {
    let inet = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from(address).to_be() },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in and sockaddr have the same size; the kernel reads
    // the address according to sin_family.
    unsafe { mem::transmute::<libc::sockaddr_in, libc::sockaddr>(inet) }
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(result) }
}

impl Rule {
    fn add(&self) -> io::Result<()> {
        let mut args = vec!["-t", self.table];
        if self.first {
            args.extend(["-I", self.chain, "1"]);
        } else {
            args.extend(["-A", self.chain]);
        }
        args.extend(self.spec.iter().map(String::as_str));
        iptables(&args)
    }

    fn delete(&self) -> io::Result<()> {
        let mut args = vec!["-t", self.table, "-D", self.chain];
        args.extend(self.spec.iter().map(String::as_str));
        iptables(&args)
    }

    /// Deletes every copy of the rule.
    fn remove_all(&self) -> io::Result<()> {
        while self.exists()? {
            self.delete()?;
        }
        Ok(())
    }

    fn exists(&self) -> io::Result<bool> {
        let mut args = vec!["-t", self.table, "-C", self.chain];
        args.extend(self.spec.iter().map(String::as_str));
        let output = run_iptables(&args)?;
        match output.status.code() {
            Some(0) => Ok(true),
            // `-C` exits with 1 when there is no such rule.
            Some(1) => Ok(false),
            _ => Err(iptables_failed(&args, &output)),
        }
    }
}

/// Runs `iptables -w ARGS`, waiting for the xtables lock.
fn iptables(args: &[&str]) -> io::Result<()> {
    let output = run_iptables(args)?;
    if output.status.success() { Ok(()) } else { Err(iptables_failed(args, &output)) }
}

fn run_iptables(args: &[&str]) -> io::Result<Output> {
    Command::new("iptables")
        .arg("-w")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("running iptables: {e}")))
}

fn iptables_failed(args: &[&str], output: &Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    io::Error::other(format!("`iptables -w {}` failed ({}): {}", args.join(" "), output.status, stderr.trim()))
}
