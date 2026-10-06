//! Host TCP → guest TCP over Firecracker's vsock: the host side of
//! Firecracker's `CONNECT <port>` / `OK <port>` handshake, then fervor's
//! forwarding preamble.

use std::io;
use std::time::Duration;

use camino::Utf8PathBuf;
use fervor_guest_abi::{Backoff, FORWARD_PORT, ForwardPreamble};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixStream};

/// Firecracker's reply to `CONNECT`: `OK <port>`, where `port` is the
/// host-side port Firecracker assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectAck {
    pub port: u32,
}

impl ConnectAck {
    /// Longest acknowledgement Firecracker sends (`OK 4294967295\n`).
    const MAX_LEN: usize = 16;

    /// Parses the reply line without its newline.
    pub fn parse(line: &str) -> Option<Self> {
        let port = line.strip_prefix("OK ")?.parse().ok()?;
        Some(Self { port })
    }

    /// Reads and parses the reply byte by byte, so nothing after its newline
    /// is consumed from the stream.
    async fn read_from(stream: &mut UnixStream) -> io::Result<Self> {
        let mut line = Vec::with_capacity(Self::MAX_LEN);
        loop {
            let byte = stream.read_u8().await?;
            if byte == b'\n' {
                break;
            }
            if line.len() == Self::MAX_LEN {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "vsock handshake reply too long"));
            }
            line.push(byte);
        }
        let line = String::from_utf8(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Self::parse(&line).ok_or_else(|| io::Error::other(format!("vsock handshake failed: `{line}`")))
    }
}

/// Opens connections to the guest's forward listener through Firecracker's
/// vsock socket.
#[derive(Debug, Clone)]
pub(crate) struct GuestConnector {
    vsock: Utf8PathBuf,
    /// The guest's forward listener comes up a moment after the host port is
    /// bound; connections accepted while it boots are retried, not reset.
    backoff: Backoff,
}

impl GuestConnector {
    pub(crate) fn new(vsock: Utf8PathBuf) -> Self {
        Self { vsock, backoff: Backoff::STARTUP }
    }

    /// Pipes `tcp` to the guest port named by `preamble` until either side closes.
    async fn forward(&self, mut tcp: TcpStream, preamble: ForwardPreamble) -> io::Result<()> {
        let mut guest = self.connect().await?;
        guest.write_all(preamble.encode().as_bytes()).await?;
        tokio::io::copy_bidirectional(&mut tcp, &mut guest).await?;
        Ok(())
    }

    async fn connect(&self) -> io::Result<UnixStream> {
        let mut retry = self.backoff.start();
        loop {
            match self.handshake().await {
                Ok(stream) => return Ok(stream),
                Err(e) => match retry.next_delay() {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => return Err(e),
                },
            }
        }
    }

    async fn handshake(&self) -> io::Result<UnixStream> {
        let mut guest = UnixStream::connect(&self.vsock).await?;
        guest.write_all(format!("CONNECT {FORWARD_PORT}\n").as_bytes()).await?;
        ConnectAck::read_from(&mut guest).await?;
        Ok(guest)
    }
}

/// One `-p HOST:GUEST` forward: accepts on the host listener and pipes each
/// connection to `guest_port` in the guest.
pub(crate) struct PortForwarder {
    listener: TcpListener,
    guest_port: u16,
    guest: GuestConnector,
}

impl PortForwarder {
    /// Pause after a failed accept (e.g. EMFILE) instead of spinning.
    const ACCEPT_ERROR_PAUSE: Duration = Duration::from_millis(100);

    pub(crate) fn new(listener: TcpListener, guest_port: u16, guest: GuestConnector) -> Self {
        Self { listener, guest_port, guest }
    }

    /// Accepts host connections until the task is aborted.
    pub(crate) async fn serve(self) {
        let guest_port = self.guest_port;
        loop {
            match self.listener.accept().await {
                Ok((tcp, peer)) => {
                    let guest = self.guest.clone();
                    tokio::spawn(async move {
                        if let Err(e) = guest.forward(tcp, ForwardPreamble { guest_port }).await {
                            tracing::warn!(%peer, guest_port, "port forward failed: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(guest_port, "accepting forwarded connection: {e}");
                    tokio::time::sleep(Self::ACCEPT_ERROR_PAUSE).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_firecracker_connect_ack() {
        let port = |line| ConnectAck::parse(line).map(|ack| ack.port);
        assert_eq!(port("OK 1073741824"), Some(1_073_741_824));
        assert_eq!(port("OK 4294967295"), Some(u32::MAX));
        assert_eq!(port("OK 4294967296"), None);
        assert_eq!(port("OK"), None);
        assert_eq!(port("OK -1"), None);
        assert_eq!(port("ERR 5"), None);
        assert_eq!(port(""), None);
    }
}
