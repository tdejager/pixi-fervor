use std::fmt;

/// An absolute, normalized path inside the guest: starts with `/`, no empty,
/// `.` or `..` components, no trailing slash (except the root itself).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GuestPath(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GuestPathError {
    #[error("guest path `{0}` must be absolute")]
    NotAbsolute(String),
    #[error("guest path `{0}` must not contain empty, `.` or `..` components")]
    NotNormalized(String),
    #[error("guest path `{0}` contains a NUL byte")]
    Nul(String),
}

impl GuestPath {
    pub fn new(path: impl Into<String>) -> Result<Self, GuestPathError> {
        let path = path.into();
        if path.contains('\0') {
            return Err(GuestPathError::Nul(path));
        }
        let Some(rest) = path.strip_prefix('/') else {
            return Err(GuestPathError::NotAbsolute(path));
        };
        if !rest.is_empty() && !rest.split('/').all(Self::is_normal_component) {
            return Err(GuestPathError::NotNormalized(path));
        }
        Ok(Self(path))
    }

    pub fn root() -> Self {
        Self("/".to_owned())
    }

    /// Appends a relative path (`a/b`, no leading slash) to this path.
    pub fn join(&self, relative: &str) -> Result<Self, GuestPathError> {
        if relative.starts_with('/') {
            return Err(GuestPathError::NotNormalized(relative.to_owned()));
        }
        if relative.is_empty() {
            return Ok(self.clone());
        }
        if self.is_root() {
            Self::new(format!("/{relative}"))
        } else {
            Self::new(format!("{}/{relative}", self.0))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The path without its leading `/`; empty for the root.
    pub fn relative(&self) -> &str {
        &self.0[1..]
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    pub fn parent(&self) -> Option<GuestPath> {
        if self.is_root() {
            return None;
        }
        let idx = self.0.rfind('/').expect("absolute path contains a slash");
        Some(if idx == 0 { Self::root() } else { Self(self.0[..idx].to_owned()) })
    }

    pub fn file_name(&self) -> Option<&str> {
        (!self.is_root()).then(|| self.0.rsplit('/').next().expect("non-empty"))
    }

    /// Component-wise prefix test: `/opt/env` is a prefix of `/opt/env/bin`
    /// but not of `/opt/environment`.
    pub fn starts_with(&self, prefix: &GuestPath) -> bool {
        prefix.is_root()
            || self.0 == prefix.0
            || (self.0.starts_with(&prefix.0) && self.0.as_bytes()[prefix.0.len()] == b'/')
    }

    fn is_normal_component(component: &str) -> bool {
        !component.is_empty() && component != "." && component != ".."
    }
}

impl fmt::Display for GuestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for GuestPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GuestPath({})", self.0)
    }
}

impl std::str::FromStr for GuestPath {
    type Err = GuestPathError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl serde::Serialize for GuestPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for GuestPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_and_unnormalized_paths() {
        assert!(GuestPath::new("opt/env").is_err());
        assert!(GuestPath::new("/opt//env").is_err());
        assert!(GuestPath::new("/opt/env/").is_err());
        assert!(GuestPath::new("/opt/../etc").is_err());
        assert!(GuestPath::new("/opt/./env").is_err());
        assert!(GuestPath::new("/opt/env").is_ok());
    }

    #[test]
    fn join_parent_and_prefix_are_component_wise() {
        let env = GuestPath::new("/opt/env").unwrap();
        let bin = env.join("bin/python").unwrap();
        assert_eq!(bin.as_str(), "/opt/env/bin/python");
        assert_eq!(bin.parent().unwrap().as_str(), "/opt/env/bin");
        assert!(bin.starts_with(&env));
        assert!(!GuestPath::new("/opt/environment").unwrap().starts_with(&env));
        assert_eq!(GuestPath::root().join("etc").unwrap().as_str(), "/etc");
        assert!(env.join("../etc").is_err());
    }
}
