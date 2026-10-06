use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::layer::EnvPrefix;
use crate::nonempty::NonEmpty;
use crate::path::GuestPath;

/// Program and arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Argv(NonEmpty<String>);

impl Argv {
    pub fn new(args: NonEmpty<String>) -> Self {
        Self(args)
    }

    pub fn args(&self) -> &[String] {
        &self.0
    }
}

/// A POSIX environment variable name: `[A-Za-z_][A-Za-z0-9_]*`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EnvName(String);

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not a valid environment variable name")]
pub struct EnvNameError(String);

impl FromStr for EnvName {
    type Err = EnvNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut chars = s.chars();
        let valid_start = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if valid_start && chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
            Ok(Self(s.to_owned()))
        } else {
            Err(EnvNameError(s.to_owned()))
        }
    }
}

impl TryFrom<String> for EnvName {
    type Error = EnvNameError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<EnvName> for String {
    fn from(name: EnvName) -> Self {
        name.0
    }
}

impl fmt::Display for EnvName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Environment variables, sorted by name for deterministic manifests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvVars(BTreeMap<EnvName, String>);

impl EnvVars {
    pub fn insert(&mut self, name: EnvName, value: String) {
        self.0.insert(name, value);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&EnvName, &String)> {
        self.0.iter()
    }
}

impl FromIterator<(EnvName, String)> for EnvVars {
    fn from_iter<I: IntoIterator<Item = (EnvName, String)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// What the guest runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entrypoint {
    argv: Argv,
    env: EnvVars,
    workdir: GuestPath,
}

impl Entrypoint {
    /// Builds the entrypoint with its complete environment: activation
    /// defaults for `prefix` first, then the user's variables on top.
    pub fn new(argv: Argv, user_env: EnvVars, workdir: GuestPath, prefix: &EnvPrefix) -> Self {
        let name = |s: &str| EnvName::from_str(s).expect("valid default name");
        let prefix = prefix.path().as_str();
        let mut env: EnvVars = [
            (name("PATH"), format!("{prefix}/bin:/usr/bin:/bin")),
            (name("CONDA_PREFIX"), prefix.to_owned()),
            (name("HOME"), "/root".to_owned()),
            (name("LANG"), "C.UTF-8".to_owned()),
        ]
        .into_iter()
        .collect();
        for (key, value) in user_env.0 {
            env.insert(key, value);
        }
        Self { argv, env, workdir }
    }

    pub fn argv(&self) -> &Argv {
        &self.argv
    }

    pub fn env(&self) -> &EnvVars {
        &self.env
    }

    pub fn workdir(&self) -> &GuestPath {
        &self.workdir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_env_overrides_activation_defaults() {
        let argv = Argv::new(NonEmpty::singleton("python".to_owned()));
        let user: EnvVars = [
            ("PATH".parse().unwrap(), "/custom".to_owned()),
            ("FLASK_APP".parse().unwrap(), "app".to_owned()),
        ]
        .into_iter()
        .collect();
        let entrypoint = Entrypoint::new(argv, user, GuestPath::root(), &EnvPrefix::default());
        let env: Vec<_> = entrypoint.env().iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        assert!(env.contains(&("PATH".into(), "/custom".into())));
        assert!(env.contains(&("CONDA_PREFIX".into(), "/opt/env".into())));
        assert!(env.contains(&("FLASK_APP".into(), "app".into())));
    }

    #[test]
    fn env_names_follow_posix() {
        assert!("_A1".parse::<EnvName>().is_ok());
        assert!("1A".parse::<EnvName>().is_err());
        assert!("A-B".parse::<EnvName>().is_err());
        assert!("".parse::<EnvName>().is_err());
    }
}
