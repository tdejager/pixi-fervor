//! Control channel: one vsock stream that the guest opens to
//! `HOST_CID:CONTROL_PORT`, carrying newline-delimited JSON messages.
//!
//! Forwarding channel: the host opens a vsock stream to `FORWARD_PORT` and
//! sends a single preamble line `FWD <guest_tcp_port>\n`; afterwards the stream
//! is spliced to `127.0.0.1:<guest_tcp_port>` inside the guest.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestToHost {
    /// The layer stack is mounted and the entrypoint has been spawned.
    Ready,
    /// The entrypoint is gone; the guest reboots right after sending this.
    Exited { report: ExitReport },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToGuest {
    /// Send SIGTERM to the entrypoint, SIGKILL after `grace_ms`, then exit.
    Shutdown { grace_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExitReport {
    Exited { code: i32 },
    Signaled { signal: i32 },
    /// Init could not start the entrypoint (mount, exec, config failure).
    InitFailed { message: String },
}

/// First line of every forwarding stream: which guest TCP port to splice to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardPreamble {
    pub guest_port: u16,
}

impl ForwardPreamble {
    /// `FWD <port>\n`, as written by the host.
    pub fn encode(self) -> String {
        format!("FWD {}\n", self.guest_port)
    }

    /// Parses a preamble line without its trailing newline.
    pub fn parse(line: &str) -> Option<Self> {
        let guest_port = line.strip_prefix("FWD ")?.trim().parse().ok()?;
        Some(Self { guest_port })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_preamble_round_trips_and_rejects_foreign_lines() {
        let preamble = ForwardPreamble { guest_port: 5000 };
        assert_eq!(ForwardPreamble::parse(preamble.encode().trim_end()), Some(preamble));
        assert_eq!(ForwardPreamble::parse("FWD 70000"), None);
        assert_eq!(ForwardPreamble::parse("CONNECT 5000"), None);
    }
}
