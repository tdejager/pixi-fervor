//! Minimal AF_VSOCK stream sockets (std has none).

use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use rustix::net::Shutdown;

/// A vsock endpoint: context id plus port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VsockAddr {
    pub cid: u32,
    pub port: u32,
}

impl VsockAddr {
    const LEN: libc::socklen_t = size_of::<libc::sockaddr_vm>() as libc::socklen_t;

    /// `port` on any local CID.
    pub fn any_cid(port: u32) -> Self {
        Self {
            cid: libc::VMADDR_CID_ANY,
            port,
        }
    }

    fn to_raw(self) -> libc::sockaddr_vm {
        libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: self.port,
            svm_cid: self.cid,
            svm_zero: [0; 4],
        }
    }

    /// A fresh, unconnected close-on-exec stream socket of this family.
    fn stream_socket() -> io::Result<OwnedFd> {
        // SAFETY: plain socket(2) call; the result is checked before use.
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a freshly created socket that nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub struct VsockStream(OwnedFd);

impl VsockStream {
    pub fn connect(addr: VsockAddr) -> io::Result<Self> {
        let socket = VsockAddr::stream_socket()?;
        let sockaddr = addr.to_raw();
        // SAFETY: `sockaddr` is a valid sockaddr_vm and its exact size is passed.
        let rc = unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&raw const sockaddr).cast(),
                VsockAddr::LEN,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(socket))
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    /// Shuts down both directions, which also wakes a reader blocked on a
    /// clone of this stream.
    pub fn shutdown(&self) -> io::Result<()> {
        rustix::net::shutdown(&self.0, Shutdown::Both)?;
        Ok(())
    }
}

impl Read for &VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        Ok(rustix::io::read(&self.0, buf)?)
    }
}

impl Write for &VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(rustix::io::write(&self.0, buf)?)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub struct VsockListener(OwnedFd);

impl VsockListener {
    pub fn bind(addr: VsockAddr) -> io::Result<Self> {
        let socket = VsockAddr::stream_socket()?;
        let sockaddr = addr.to_raw();
        // SAFETY: `sockaddr` is a valid sockaddr_vm and its exact size is passed.
        let rc = unsafe {
            libc::bind(
                socket.as_raw_fd(),
                (&raw const sockaddr).cast(),
                VsockAddr::LEN,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        rustix::net::listen(&socket, 64)?;
        Ok(Self(socket))
    }

    pub fn accept(&self) -> io::Result<VsockStream> {
        let fd = rustix::net::accept_with(self.0.as_fd(), rustix::net::SocketFlags::CLOEXEC)?;
        Ok(VsockStream(fd))
    }
}
