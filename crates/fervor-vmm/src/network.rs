//! `--network nat`: the guest's `eth0` is backed by a TAP device on the run
//! host, which routes and masquerades the guest's traffic.
//!
//! Each run claims a slot N by binding the abstract Unix socket
//! `fervor/net/<N>` and holding it for the whole run; the first bindable N
//! wins. Slot N owns the TAP device `fervor<N>` and the /30 at
//! `172.29.0.0 + 4N`: the host's end is `.1 + 4N`, the guest's `.2 + 4N`.
//!
//! Forwarding is outbound-only: the guest may reach anything but another
//! guest (`fervor+`), and only replies come back in. New inbound connections
//! and guest-to-guest traffic are dropped whatever the host's `FORWARD`
//! policy, and whatever order concurrent runs insert their rules in, since
//! each slot's rules only match its own TAP device.

// The pure parts below compile everywhere (and are tested everywhere); only
// Linux run hosts use them.
#![cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]

use std::net::Ipv4Addr;

#[cfg(target_os = "linux")]
mod nat;

#[cfg(target_os = "linux")]
pub(crate) use nat::NatNetwork;

/// The /16 the per-run subnets are carved from.
const SUBNET_BASE: Ipv4Addr = Ipv4Addr::new(172, 29, 0, 0);
const PREFIX_LEN: u8 = 30;
/// Number of /30 subnets in the /16.
const SLOTS: u16 = 1 << 14;

/// The host's resolver configuration, copied into the guest.
const HOST_RESOLV_CONF: &str = "/etc/resolv.conf";
/// systemd-resolved's upstream servers, for hosts whose resolv.conf only
/// names its loopback stub (e.g. Ubuntu, Lima).
const RESOLVED_RESOLV_CONF: &str = "/run/systemd/resolve/resolv.conf";

/// NAT networking needs a Linux run host, like KVM, which is checked first:
/// never constructed elsewhere.
#[cfg(not(target_os = "linux"))]
pub(crate) enum NatNetwork {}

#[cfg(not(target_os = "linux"))]
impl NatNetwork {
    pub(crate) fn check_permission() -> Result<(), crate::RunError> {
        Err(crate::RunError::infrastructure("--network nat", "needs a Linux run host"))
    }

    pub(crate) fn setup() -> Result<Self, crate::RunError> {
        Err(crate::RunError::infrastructure("--network nat", "needs a Linux run host"))
    }

    pub(crate) fn interface(&self) -> crate::firecracker::NetworkInterface {
        match *self {}
    }

    pub(crate) fn guest_config(&self, _resolv_conf: String) -> fervor_guest_abi::NetworkConfig {
        match *self {}
    }
}

/// One run's TAP device and subnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot(u16);

impl Slot {
    fn tap_name(self) -> String {
        format!("fervor{}", self.0)
    }

    fn nth_address(self, n: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(SUBNET_BASE) + 4 * u32::from(self.0) + n)
    }

    fn subnet(self) -> Ipv4Addr {
        self.nth_address(0)
    }

    /// The host's end of the link: the guest's gateway.
    fn host(self) -> Ipv4Addr {
        self.nth_address(1)
    }

    fn guest(self) -> Ipv4Addr {
        self.nth_address(2)
    }

    /// Locally administered unicast MAC derived from the guest's address.
    fn guest_mac(self) -> [u8; 6] {
        let [a, b, c, d] = self.guest().octets();
        [0x06, 0x00, a, b, c, d]
    }

    fn netmask() -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX << (32 - PREFIX_LEN))
    }

    /// The rules for this slot, in the order they are added; removed in
    /// reverse. See [`Rule`].
    fn rules(self) -> [Rule; 4] {
        let tap = self.tap_name();
        let subnet = format!("{}/{PREFIX_LEN}", self.subnet());
        let comment = format!("fervor-{tap}");
        let rule = |table, chain, first, matches: &[&str], target| Rule {
            table,
            chain,
            first,
            spec: matches.iter().copied().chain(["-m", "comment", "--comment", &comment, "-j", target]).map(str::to_owned).collect(),
        };
        [
            // Masquerade the guest's traffic leaving through any other interface.
            rule("nat", "POSTROUTING", false, &["-s", &subnet, "!", "-o", &tap], "MASQUERADE"),
            // Each insert lands above the previous one: replies are accepted
            // before everything else towards the guest is dropped.
            rule("filter", "FORWARD", true, &["-o", &tap], "DROP"),
            rule("filter", "FORWARD", true, &["-o", &tap, "-m", "conntrack", "--ctstate", "RELATED,ESTABLISHED"], "ACCEPT"),
            // Out to anywhere but another guest.
            rule("filter", "FORWARD", true, &["-i", &tap, "!", "-o", "fervor+"], "ACCEPT"),
        ]
    }
}

/// One `iptables` rule, tagged with a comment naming the TAP device. Fully
/// determined by the slot, so a crashed run's leftovers can be found again.
struct Rule {
    table: &'static str,
    chain: &'static str,
    /// Insert at the top of the chain instead of appending, so a DROP rule
    /// or policy set up by others (e.g. Docker) does not win.
    first: bool,
    spec: Vec<String>,
}

/// The guest's `/etc/resolv.conf` from this host's resolver configuration;
/// empty, with a warning, when it names no nameserver the guest can reach.
pub(crate) fn host_resolv_conf() -> String {
    let read = |path| std::fs::read_to_string(path).ok();
    guest_resolv_conf(read(HOST_RESOLV_CONF).as_deref(), read(RESOLVED_RESOLV_CONF).as_deref()).unwrap_or_else(|| {
        tracing::warn!("neither {HOST_RESOLV_CONF} nor {RESOLVED_RESOLV_CONF} names an IPv4 nameserver the guest can reach; the guest has no DNS");
        String::new()
    })
}

/// The guest's `/etc/resolv.conf`: the host's (`host`), or, when that names
/// no nameserver the guest can reach, systemd-resolved's upstream one
/// (`fallback`). `None` when neither has one.
fn guest_resolv_conf(host: Option<&str>, fallback: Option<&str>) -> Option<String> {
    [host, fallback].into_iter().flatten().find_map(reachable_resolv_conf)
}

/// `contents` without what the guest cannot use: loopback, unspecified and
/// IPv6 nameservers (the guest is IPv4-only), comments and unknown lines.
/// `search`, `domain` and `options` stay (cluster DNS relies on them).
/// `None` when no nameserver is left.
fn reachable_resolv_conf(contents: &str) -> Option<String> {
    let mut out = String::new();
    let mut nameservers = 0;
    for line in contents.lines() {
        let mut words = line.split_whitespace();
        let keep = match words.next() {
            Some("nameserver") => {
                let reachable = words
                    .next()
                    .and_then(|address| address.parse::<Ipv4Addr>().ok())
                    .is_some_and(|address| !address.is_loopback() && !address.is_unspecified());
                nameservers += usize::from(reachable);
                reachable
            }
            Some("search" | "domain" | "options") => true,
            _ => false,
        };
        if keep {
            out.push_str(line.trim());
            out.push('\n');
        }
    }
    (nameservers > 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_map_to_disjoint_30s_in_the_16() {
        let first = Slot(0);
        assert_eq!((first.subnet(), first.host(), first.guest()), (Ipv4Addr::new(172, 29, 0, 0), Ipv4Addr::new(172, 29, 0, 1), Ipv4Addr::new(172, 29, 0, 2)));
        assert_eq!(first.tap_name(), "fervor0");
        assert_eq!(first.guest_mac(), [0x06, 0x00, 172, 29, 0, 2]);

        let second = Slot(1);
        assert_eq!((second.subnet(), second.host(), second.guest()), (Ipv4Addr::new(172, 29, 0, 4), Ipv4Addr::new(172, 29, 0, 5), Ipv4Addr::new(172, 29, 0, 6)));
        let third_octet = Slot(64);
        assert_eq!(third_octet.host(), Ipv4Addr::new(172, 29, 1, 1));

        let last = Slot(SLOTS - 1);
        assert_eq!((last.subnet(), last.guest()), (Ipv4Addr::new(172, 29, 255, 252), Ipv4Addr::new(172, 29, 255, 254)));
        assert!(last.tap_name().len() < 16, "fits IFNAMSIZ");
        assert_eq!(Slot::netmask(), Ipv4Addr::new(255, 255, 255, 252));
    }

    #[test]
    fn rules_forward_outbound_only_and_accept_replies_above_the_drop() {
        let rules = Slot(1).rules();
        let shown: Vec<_> = rules.iter().map(|rule| (rule.table, rule.chain, rule.first, rule.spec.join(" "))).collect();
        let tag = "-m comment --comment fervor-fervor1";
        assert_eq!(
            shown,
            [
                ("nat", "POSTROUTING", false, format!("-s 172.29.0.4/30 ! -o fervor1 {tag} -j MASQUERADE")),
                ("filter", "FORWARD", true, format!("-o fervor1 {tag} -j DROP")),
                ("filter", "FORWARD", true, format!("-o fervor1 -m conntrack --ctstate RELATED,ESTABLISHED {tag} -j ACCEPT")),
                ("filter", "FORWARD", true, format!("-i fervor1 ! -o fervor+ {tag} -j ACCEPT")),
            ]
        );
    }

    #[test]
    fn resolv_conf_drops_loopback_and_ipv6_nameservers_but_keeps_search_and_options() {
        let host = "# managed by someone\n\
                    nameserver 127.0.0.53\n\
                    nameserver ::1\n\
                    nameserver 2001:db8::1\n\
                    nameserver 0.0.0.0\n\
                    nameserver 10.96.0.10\n\
                    search default.svc.cluster.local svc.cluster.local cluster.local\n\
                    options ndots:5\n\
                    sortlist 130.155.160.0/255.255.240.0\n";
        assert_eq!(
            guest_resolv_conf(Some(host), Some("nameserver 192.168.5.3\n")).as_deref(),
            Some("nameserver 10.96.0.10\nsearch default.svc.cluster.local svc.cluster.local cluster.local\noptions ndots:5\n")
        );
    }

    #[test]
    fn resolv_conf_falls_back_when_only_loopback_nameservers_remain() {
        let stub = "nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch .\n";
        let upstream = "# resolved upstream\nnameserver 192.168.5.3\nnameserver fe80::1%eth0\nsearch lima.internal\n";
        assert_eq!(guest_resolv_conf(Some(stub), Some(upstream)).as_deref(), Some("nameserver 192.168.5.3\nsearch lima.internal\n"));
        assert_eq!(guest_resolv_conf(None, Some(upstream)).as_deref(), Some("nameserver 192.168.5.3\nsearch lima.internal\n"));
        assert_eq!(guest_resolv_conf(Some(stub), Some("nameserver ::1\n")), None);
        assert_eq!(guest_resolv_conf(Some(stub), None), None);
    }
}
