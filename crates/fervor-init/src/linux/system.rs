//! Host identity and the loopback interface.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use rustix::net::{AddressFamily, SocketType};

use super::{Context, Error};

/// The guest's hostname; the boot layer's `/etc/hostname` is its single
/// source.
pub struct Hostname(String);

impl Hostname {
    const FILE: &str = "/etc/hostname";

    pub fn from_boot_layer() -> Result<Self, Error> {
        let name =
            std::fs::read_to_string(Self::FILE).context(|| format!("reading {}", Self::FILE))?;
        Ok(Self(name.trim().to_owned()))
    }

    pub fn apply(&self) -> Result<(), Error> {
        let name = &self.0;
        rustix::system::sethostname(name.as_bytes())
            .context(|| format!("setting the hostname to `{name}`"))
    }
}

/// The `lo` interface. Port forwarding connects to 127.0.0.1, so it must be
/// up.
pub struct Loopback;

impl Loopback {
    /// Brings `lo` up; the kernel assigns 127.0.0.1 and ::1 on its own.
    pub fn up() -> Result<(), Error> {
        let context = || "bringing the loopback interface up".to_owned();
        let socket =
            rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None).context(context)?;

        // SAFETY: `ifreq` is plain old data; all-zero is a valid value.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        for (dst, src) in request.ifr_name.iter_mut().zip(b"lo") {
            *dst = *src as libc::c_char;
        }
        Self::ioctl(&socket, libc::SIOCGIFFLAGS, &mut request).context(context)?;
        // SAFETY: SIOCGIFFLAGS filled in the `ifru_flags` member of the union.
        unsafe { request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
        Self::ioctl(&socket, libc::SIOCSIFFLAGS, &mut request).context(context)
    }

    fn ioctl(socket: &OwnedFd, request: libc::Ioctl, ifreq: &mut libc::ifreq) -> io::Result<()> {
        // SAFETY: the socket is open and `ifreq` is a valid, NUL-terminated
        // ifreq that outlives the call.
        if unsafe { libc::ioctl(socket.as_raw_fd(), request, ifreq) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
