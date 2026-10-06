use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A size in bytes. Parses `1048576`, `512KiB`, `1MiB`, `2MB`, `1GiB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByteSize(u64);

impl ByteSize {
    pub const fn bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn mib(mib: u64) -> Self {
        Self(mib * 1024 * 1024)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

#[derive(Debug, thiserror::Error)]
#[error("`{0}` is not a size (expected e.g. `1MiB`, `512KiB`, `2MB` or a byte count)")]
pub struct ByteSizeParseError(String);

impl FromStr for ByteSize {
    type Err = ByteSizeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ByteSizeParseError(s.to_owned());
        let trimmed = s.trim();
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (number, unit) = trimmed.split_at(split);
        let number: u64 = number.parse().map_err(|_| err())?;
        let factor: u64 = match unit.trim() {
            "" | "B" => 1,
            "KiB" => 1 << 10,
            "MiB" => 1 << 20,
            "GiB" => 1 << 30,
            "KB" | "kB" => 1_000,
            "MB" => 1_000_000,
            "GB" => 1_000_000_000,
            _ => return Err(err()),
        };
        number.checked_mul(factor).map(Self).ok_or_else(err)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
        for (unit, factor) in UNITS {
            if self.0 >= factor {
                return write!(f, "{:.1} {unit}", self.0 as f64 / factor as f64);
            }
        }
        write!(f, "{} B", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_and_decimal_units() {
        assert_eq!("1MiB".parse::<ByteSize>().unwrap(), ByteSize::mib(1));
        assert_eq!("2MB".parse::<ByteSize>().unwrap(), ByteSize::bytes(2_000_000));
        assert_eq!("4096".parse::<ByteSize>().unwrap(), ByteSize::bytes(4096));
        assert!("1 TiB".parse::<ByteSize>().is_err());
        assert!("MiB".parse::<ByteSize>().is_err());
    }
}
