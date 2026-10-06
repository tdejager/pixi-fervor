//! Host → guest port forwarding: every vsock connection on `FORWARD_PORT`
//! names a guest TCP port in its preamble and is then spliced to it.

use std::io::{self, Read};
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::thread;
use std::time::Duration;

use fervor_guest_abi::{Backoff, FORWARD_PORT, ForwardPreamble};

use super::vsock::{VsockAddr, VsockListener, VsockStream};
use super::{Context, Error};

/// Accepts forwarding connections from the host, one session thread each.
pub struct ForwardListener(VsockListener);

impl ForwardListener {
    /// Keeps a persistent accept failure (e.g. out of fds) from spinning.
    const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

    pub fn bind() -> Result<Self, Error> {
        VsockListener::bind(VsockAddr::any_cid(FORWARD_PORT))
            .map(Self)
            .context(|| format!("listening on vsock port {FORWARD_PORT}"))
    }

    /// Accepts connections on a background thread for the life of the guest.
    pub fn spawn(self) {
        thread::spawn(move || {
            loop {
                match self.0.accept() {
                    Ok(vsock) => {
                        thread::spawn(move || {
                            if let Err(err) = ForwardSession(vsock).run() {
                                eprintln!("fervor-init: port forward: {err}");
                            }
                        });
                    }
                    Err(err) => {
                        eprintln!("fervor-init: accepting a forwarded connection failed: {err}");
                        thread::sleep(Self::ACCEPT_BACKOFF);
                    }
                }
            }
        });
    }
}

/// One forwarded connection: preamble, then a splice to the guest port.
struct ForwardSession(VsockStream);

impl ForwardSession {
    /// `FWD 65535` plus slack; a longer first line is not a preamble.
    const MAX_PREAMBLE: usize = 32;

    fn run(self) -> io::Result<()> {
        let vsock = self.0;
        let preamble = Self::read_preamble(&vsock)?;
        let port = preamble.guest_port;
        let tcp = Self::connect_guest(port).map_err(|err| {
            io::Error::new(err.kind(), format!("connecting to 127.0.0.1:{port}: {err}"))
        })?;

        let upstream = {
            let vsock = vsock.try_clone()?;
            let mut tcp = tcp.try_clone()?;
            thread::spawn(move || {
                let copied = io::copy(&mut &vsock, &mut tcp);
                // Pass the host's EOF on, even if the copy failed midway.
                let _ = tcp.shutdown(Shutdown::Write);
                copied
            })
        };
        let downstream = io::copy(&mut &tcp, &mut &vsock);
        // Firecracker does not relay a guest half-close to the host's Unix
        // socket, so the host only sees EOF once both directions are shut
        // down. Once the guest service is done sending, the connection is
        // over; this also ends the upstream copy.
        let _ = vsock.shutdown();
        let upstream = upstream
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("forwarding thread panicked")));
        downstream.and(upstream).map(drop)
    }

    /// Clients may connect while the entrypoint is still starting; a refused
    /// connection is retried within the startup window before the client is
    /// dropped.
    fn connect_guest(port: u16) -> io::Result<TcpStream> {
        let mut retry = Backoff::STARTUP.start();
        loop {
            match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
                    match retry.next_delay() {
                        Some(delay) => thread::sleep(delay),
                        None => return Err(err),
                    }
                }
                result => return result,
            }
        }
    }

    /// Reads the `FWD <port>` line byte by byte so nothing after it is
    /// consumed.
    fn read_preamble(vsock: &VsockStream) -> io::Result<ForwardPreamble> {
        let mut line = Vec::with_capacity(Self::MAX_PREAMBLE);
        let mut byte = [0u8; 1];
        loop {
            if (&*vsock).read(&mut byte)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed before the preamble",
                ));
            }
            if byte[0] == b'\n' {
                break;
            }
            if line.len() == Self::MAX_PREAMBLE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "preamble line too long",
                ));
            }
            line.push(byte[0]);
        }
        std::str::from_utf8(&line)
            .ok()
            .and_then(ForwardPreamble::parse)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bad preamble {:?}", String::from_utf8_lossy(&line)),
                )
            })
    }
}
