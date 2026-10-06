use serde::{Deserialize, Serialize};

/// What `fervor-init` runs once the layer stack is assembled.
///
/// Embedded in the pack header, so it is per image, never per run: run-time
/// settings (port forwards, resources) live on the host side only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestConfig {
    /// Program and arguments; `argv[0]` is resolved against `PATH` from `env`.
    pub argv: Vec<String>,
    /// Complete environment of the entrypoint, sorted by name.
    pub env: Vec<(String, String)>,
    /// Absolute working directory inside the guest.
    pub workdir: String,
    /// Size limit of the tmpfs that backs the writable overlay layer.
    pub scratch_size_mib: u32,
}
