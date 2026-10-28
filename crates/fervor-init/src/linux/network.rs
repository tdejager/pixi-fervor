//! Network interfaces: `lo` always, `eth0` when the run has a network, via
//! the classic interface and routing ioctls.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, OwnedFd};

use fervor_guest_abi::NetworkConfig;
use rustix::net::{AddressFamily, SocketType};

use super::{Context, Error};
use crate::plan::Ipv4Setup;

/// The `lo` interface. Port forwarding connects to 127.0.0.1, so it must be
/// up.
pub struct Loopback;

impl Loopback {
    /// Brings `lo` up; the kernel assigns 127.0.0.1 and ::1 on its own.
    pub fn up() -> Result<(), Error> {
        let context = || "bringing the loopback interface up".to_owned();
        InterfaceSocket::open()
            .context(context)?
            .up("lo")
            .context(context)
    }
}

/// `eth0`, the virtio-net device backed by the run host's TAP device.
pub struct Ethernet;

impl Ethernet {
    const NAME: &str = "eth0";
    const RESOLV_CONF: &str = "/etc/resolv.conf";

    /// Configures the address and default route of `eth0` and writes
    /// `/etc/resolv.conf` (in the overlay root, so after the switch).
    pub fn configure(config: &NetworkConfig) -> Result<(), Error> {
        let setup = Ipv4Setup::of(config)
            .map_err(|err| Error::msg(format!("invalid network config: {err}")))?;
        let Ipv4Setup {
            address,
            netmask,
            gateway,
        } = setup;
        let name = Self::NAME;
        let prefix_len = config.prefix_len;
        let socket = InterfaceSocket::open().context(|| format!("configuring {name}"))?;
        socket
            .set_ipv4(name, libc::SIOCSIFADDR, address)
            .context(|| format!("setting the address of {name} to {address}/{prefix_len}"))?;
        socket
            .set_ipv4(name, libc::SIOCSIFNETMASK, netmask)
            .context(|| format!("setting the netmask of {name} to {netmask}"))?;
        socket.up(name).context(|| format!("bringing {name} up"))?;
        socket
            .add_default_route(name, gateway)
            .context(|| format!("adding the default route via {gateway} on {name}"))?;
        Self::write_resolv_conf(&config.resolv_conf)
    }

    fn write_resolv_conf(contents: &str) -> Result<(), Error> {
        let file = Self::RESOLV_CONF;
        let context = || format!("writing {file}");
        std::fs::create_dir_all("/etc").context(context)?;
        // A symlink (e.g. into systemd-resolved's /run directory) would
        // redirect the write; replace it with a plain file.
        match std::fs::remove_file(file) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return Err(err).context(context);
            }
            _ => {}
        }
        std::fs::write(file, contents).context(context)
    }
}

/// An IPv4 datagram socket, the handle for interface and route ioctls.
struct InterfaceSocket(OwnedFd);

impl InterfaceSocket {
    fn open() -> io::Result<Self> {
        Ok(Self(rustix::net::socket(
            AddressFamily::INET,
            SocketType::DGRAM,
            None,
        )?))
    }

    fn up(&self, name: &str) -> io::Result<()> {
        let mut request = Self::ifreq(name);
        self.ioctl(libc::SIOCGIFFLAGS, &mut request)?;
        // SAFETY: SIOCGIFFLAGS filled in the `ifru_flags` member of the union.
        unsafe { request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
        self.ioctl(libc::SIOCSIFFLAGS, &mut request)
    }

    /// Sets an address-valued interface attribute (address or netmask).
    fn set_ipv4(&self, name: &str, request: libc::Ioctl, ip: Ipv4Addr) -> io::Result<()> {
        let mut ifreq = Self::ifreq(name);
        ifreq.ifr_ifru.ifru_addr = Self::sockaddr(ip);
        self.ioctl(request, &mut ifreq)
    }

    fn add_default_route(&self, name: &str, gateway: Ipv4Addr) -> io::Result<()> {
        let mut device = Self::ifreq(name).ifr_name;
        let route = RtEntry {
            rt_pad1: 0,
            rt_dst: Self::sockaddr(Ipv4Addr::UNSPECIFIED),
            rt_gateway: Self::sockaddr(gateway),
            rt_genmask: Self::sockaddr(Ipv4Addr::UNSPECIFIED),
            rt_flags: libc::RTF_UP | libc::RTF_GATEWAY,
            rt_pad2: 0,
            rt_pad3: 0,
            rt_pad4: std::ptr::null_mut(),
            rt_metric: 0,
            rt_dev: device.as_mut_ptr(),
            rt_mtu: 0,
            rt_window: 0,
            rt_irtt: 0,
        };
        // SAFETY: `route` is a correctly laid out `struct rtentry` whose
        // device name is NUL-terminated and outlives the call.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), libc::SIOCADDRT, &route) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn ifreq(name: &str) -> libc::ifreq {
        // SAFETY: `ifreq` is plain old data; all-zero is a valid value.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        // Interface names are short constants; the last byte stays NUL.
        debug_assert!(name.len() < request.ifr_name.len());
        for (dst, src) in request.ifr_name.iter_mut().zip(name.as_bytes()) {
            *dst = *src as libc::c_char;
        }
        request
    }

    fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr {
        let sin = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from(ip).to_be(),
            },
            sin_zero: [0; 8],
        };
        // SAFETY: `sockaddr_in` and `sockaddr` have the same size, and the
        // kernel reads it as `sockaddr_in` because of its family.
        unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
    }

    fn ioctl(&self, request: libc::Ioctl, ifreq: &mut libc::ifreq) -> io::Result<()> {
        // SAFETY: the socket is open and `ifreq` is a valid, NUL-terminated
        // ifreq that outlives the call.
        if unsafe { libc::ioctl(self.0.as_raw_fd(), request, ifreq) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// `struct rtentry` from `<linux/route.h>` (glibc's libc crate lacks it).
#[repr(C)]
struct RtEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_pad4: *mut libc::c_void,
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

// Guests are 64-bit (aarch64, x86_64).
const _: () = assert!(std::mem::size_of::<RtEntry>() == 120);
