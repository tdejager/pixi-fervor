//! Host identity.

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
