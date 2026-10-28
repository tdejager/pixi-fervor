//! Processes that run next to the entrypoint: supervised services and the
//! journal daemon that collects their output.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::entrypoint::Argv;
use crate::layer::{EnvPrefix, LayerIndex};
use crate::path::GuestPath;

/// Where the `systemd-journald` package installs the daemon, relative to the prefix.
pub const JOURNALD_PATH: &str = "lib/systemd/systemd-journald";
/// Name of the journal daemon's process; no service may take it.
const JOURNALD_NAME: &str = "systemd-journald";
/// journald cuts stream identifiers after 253 bytes; longer names would not
/// be found under their own name in the journal.
const MAX_NAME_LEN: usize = 253;

/// A service's name: `[A-Za-z0-9_.-]+`, not `systemd-journald`. It is the
/// journal identifier of the service's output.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceName(String);

#[derive(Debug, thiserror::Error)]
pub enum ServiceNameError {
    #[error("`{0}` is not a valid service name (expected at most {MAX_NAME_LEN} letters, digits, `_`, `.` or `-`)")]
    Invalid(String),
    #[error("`{JOURNALD_NAME}` is reserved for the journal daemon (use `--journal`)")]
    Reserved,
}

impl FromStr for ServiceName {
    type Err = ServiceNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
        if s.is_empty() || s.len() > MAX_NAME_LEN || !s.chars().all(valid) {
            return Err(ServiceNameError::Invalid(s.to_owned()));
        }
        if s == JOURNALD_NAME {
            return Err(ServiceNameError::Reserved);
        }
        Ok(Self(s.to_owned()))
    }
}

impl TryFrom<String> for ServiceName {
    type Error = ServiceNameError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ServiceName> for String {
    fn from(name: ServiceName) -> Self {
        name.0
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A long-running process, restarted whenever it exits. It shares the
/// entrypoint's environment and working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Service {
    pub name: ServiceName,
    pub argv: Argv,
}

/// An image's services in start order.
///
/// Invariant: names are unique.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Service>", into = "Vec<Service>")]
pub struct Services(Vec<Service>);

#[derive(Debug, thiserror::Error)]
#[error("service `{0}` is defined more than once")]
pub struct DuplicateService(ServiceName);

impl Services {
    pub fn new(services: Vec<Service>) -> Result<Self, DuplicateService> {
        let mut names = BTreeSet::new();
        if let Some(duplicate) = services.iter().find(|s| !names.insert(&s.name)) {
            return Err(DuplicateService(duplicate.name.clone()));
        }
        Ok(Self(services))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Service> {
        self.0.iter()
    }
}

impl TryFrom<Vec<Service>> for Services {
    type Error = DuplicateService;

    fn try_from(services: Vec<Service>) -> Result<Self, Self::Error> {
        Self::new(services)
    }
}

impl From<Services> for Vec<Service> {
    fn from(services: Services) -> Self {
        services.0
    }
}

/// `systemd-journald`, started before every other process; the output of
/// the services and the entrypoint goes to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    /// Absolute guest path of the daemon.
    pub daemon: GuestPath,
}

#[derive(Debug, thiserror::Error)]
#[error("--journal needs `{0}` in the image, but no layer provides it; add the `systemd-journald` package (e.g. `-s systemd-journald`)")]
pub struct JournaldMissing(GuestPath);

impl Journal {
    /// The daemon installed in `prefix`, provided its path is in one of the
    /// stack's layer `indices`.
    pub fn locate<'a>(prefix: &EnvPrefix, indices: impl IntoIterator<Item = &'a LayerIndex>) -> Result<Self, JournaldMissing> {
        let daemon = prefix.path().join(JOURNALD_PATH).expect("valid relative path");
        let present = indices.into_iter().flat_map(|index| &index.entries).any(|entry| entry.path == daemon);
        if present { Ok(Self { daemon }) } else { Err(JournaldMissing(daemon)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileOwner;
    use crate::layer::IndexEntry;
    use crate::nonempty::NonEmpty;

    fn service(name: &str) -> Service {
        Service { name: name.parse().unwrap(), argv: Argv::new(NonEmpty::singleton("sleep".to_owned())) }
    }

    #[test]
    fn names_are_restricted_and_journald_is_reserved() {
        for ok in ["tick", "web-1", "a.b_c", "X9"] {
            assert!(ok.parse::<ServiceName>().is_ok(), "{ok}");
        }
        for bad in ["", "a b", "a/b", "tick=1", "ü"] {
            assert!(matches!(bad.parse::<ServiceName>(), Err(ServiceNameError::Invalid(_))), "{bad}");
        }
        assert!(matches!("systemd-journald".parse::<ServiceName>(), Err(ServiceNameError::Reserved)));
        assert!("a".repeat(MAX_NAME_LEN).parse::<ServiceName>().is_ok());
        assert!(matches!("a".repeat(MAX_NAME_LEN + 1).parse::<ServiceName>(), Err(ServiceNameError::Invalid(_))));
    }

    #[test]
    fn services_keep_order_and_reject_duplicate_names() {
        let services = Services::new(vec![service("b"), service("a")]).unwrap();
        let names: Vec<String> = services.iter().map(|s| s.name.to_string()).collect();
        assert_eq!(names, ["b", "a"]);
        assert!(Services::new(vec![service("a"), service("b"), service("a")]).is_err());
        let json = r#"[{"name":"a","argv":["x"]},{"name":"a","argv":["y"]}]"#;
        assert!(serde_json::from_str::<Services>(json).is_err());
    }

    #[test]
    fn journal_requires_the_daemon_in_the_stack() {
        let prefix = EnvPrefix::default();
        let index = |path: &str| LayerIndex {
            entries: vec![IndexEntry { path: GuestPath::new(path).unwrap(), owner: FileOwner::Package("p".into()) }],
        };
        let other = index("/opt/env/bin/journalctl");
        let daemon = index("/opt/env/lib/systemd/systemd-journald");

        let journal = Journal::locate(&prefix, [&other, &daemon]).unwrap();
        assert_eq!(journal.daemon.as_str(), "/opt/env/lib/systemd/systemd-journald");
        let err = Journal::locate(&prefix, [&other]).unwrap_err();
        assert!(err.to_string().contains("/opt/env/lib/systemd/systemd-journald"), "{err}");
        assert!(err.to_string().contains("systemd-journald` package"), "{err}");
    }
}
