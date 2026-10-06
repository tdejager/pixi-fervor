//! The control channel to the host: newline-delimited JSON over vsock.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use fervor_guest_abi::{Backoff, CONTROL_PORT, GuestToHost, HOST_CID, HostToGuest};

use super::entrypoint::Supervisor;
use super::vsock::{VsockAddr, VsockStream};

/// The host listens before it boots the VM; the retries only cover the
/// virtio-vsock transport coming up (about 20 attempts, 100 ms apart).
const CONNECT_BACKOFF: Backoff = Backoff {
    initial: Duration::from_millis(100),
    max_delay: Duration::from_millis(100),
    window: Duration::from_secs(2),
};

pub struct Control {
    stream: Mutex<VsockStream>,
}

impl Control {
    /// Connects to the host, or reports on the console and returns `None`:
    /// the guest still runs without a control channel.
    pub fn connect() -> Option<Self> {
        let host = VsockAddr {
            cid: HOST_CID,
            port: CONTROL_PORT,
        };
        let mut retry = CONNECT_BACKOFF.start();
        loop {
            match VsockStream::connect(host) {
                Ok(stream) => {
                    return Some(Self {
                        stream: Mutex::new(stream),
                    });
                }
                Err(err) => match retry.next_delay() {
                    Some(delay) => thread::sleep(delay),
                    None => {
                        eprintln!(
                            "fervor-init: no control channel to the host (vsock {HOST_CID}:{CONTROL_PORT}): {err}"
                        );
                        return None;
                    }
                },
            }
        }
    }

    pub fn send(&self, message: &GuestToHost) {
        let mut line = serde_json::to_vec(message).expect("control messages always serialize");
        line.push(b'\n');
        let stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(err) = (&*stream).write_all(&line) {
            eprintln!("fervor-init: sending {message:?} to the host failed: {err}");
        }
    }

    /// Handles host requests on a background thread until the host closes
    /// the channel.
    pub fn spawn_reader(&self, supervisor: Arc<Supervisor>) {
        let stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_clone();
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("fervor-init: cannot read host requests: {err}");
                return;
            }
        };
        thread::spawn(move || {
            for line in BufReader::new(&stream).lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(err) => {
                        eprintln!("fervor-init: control channel read failed: {err}");
                        return;
                    }
                };
                match serde_json::from_str::<HostToGuest>(&line) {
                    Ok(HostToGuest::Shutdown { grace_ms }) => {
                        supervisor.request_shutdown(Duration::from_millis(grace_ms));
                    }
                    Err(err) => {
                        eprintln!("fervor-init: ignoring malformed host message {line:?}: {err}")
                    }
                }
            }
        });
    }
}
