//! Pure decisions of the boot sequence, kept apart from the syscalls so they
//! can be tested on any host.

use std::fmt;
use std::path::{Path, PathBuf};

use fervor_guest_abi::PackEntry;
use fervor_guest_abi::pack::PACK_ALIGN;

/// The entrypoint's own `PATH` (not init's), used to resolve `argv[0]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchPath<'a> {
    var: Option<&'a str>,
}

impl<'a> SearchPath<'a> {
    pub fn new(var: Option<&'a str>) -> Self {
        Self { var }
    }

    /// The `PATH` entry of an environment, if any.
    pub fn from_env(env: &'a [(String, String)]) -> Self {
        Self::new(
            env.iter()
                .find(|(name, _)| name == "PATH")
                .map(|(_, value)| value.as_str()),
        )
    }

    /// Resolves `program` the way `execvp` would: a name containing `/` is
    /// used as is, otherwise the first executable `<dir>/<name>` along the
    /// path wins. Empty entries are skipped instead of meaning "current
    /// directory".
    pub fn resolve(&self, program: &str, is_executable: impl Fn(&Path) -> bool) -> Option<PathBuf> {
        if program.is_empty() {
            return None;
        }
        if program.contains('/') {
            return Some(PathBuf::from(program));
        }
        self.var?
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join(program))
            .find(|candidate| is_executable(candidate))
    }
}

/// The raw `PATH` value, or `unset`.
impl fmt::Display for SearchPath<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.var.unwrap_or("unset"))
    }
}

/// Overlay lower directories, uppermost first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLowers(Vec<PathBuf>);

impl OverlayLowers {
    /// The pack layers (given bottom → top, as in the pack header) reversed,
    /// then the boot root at the bottom.
    pub fn new(layer_mounts: &[PathBuf], boot_root: &Path) -> Self {
        Self(
            layer_mounts
                .iter()
                .rev()
                .cloned()
                .chain(std::iter::once(boot_root.to_path_buf()))
                .collect(),
        )
    }

    pub fn as_slice(&self) -> &[PathBuf] {
        &self.0
    }
}

/// The byte range of the pack a layer's loop device exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopRange {
    pub offset: u64,
    /// The loop driver drops a trailing partial 512-byte sector, which would
    /// cut off the end of an image whose length is not sector aligned; every
    /// layer owns its slot up to the next `PACK_ALIGN` boundary (zero
    /// padding), so the range ends at the slot end instead.
    pub size_limit: u64,
}

impl LoopRange {
    pub fn of(entry: &PackEntry) -> Self {
        Self {
            offset: entry.offset,
            size_limit: entry.len.div_ceil(PACK_ALIGN) * PACK_ALIGN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_bare_names_along_path_in_order() {
        let executables = ["/opt/env/bin/python", "/usr/bin/python"];
        let is_exec = |p: &Path| executables.iter().any(|e| Path::new(e) == p);
        let path = |var| SearchPath::new(Some(var));
        assert_eq!(
            path(":/missing:/opt/env/bin:/usr/bin").resolve("python", is_exec),
            Some(PathBuf::from("/opt/env/bin/python"))
        );
        assert_eq!(
            path("/usr/bin:/opt/env/bin").resolve("python", is_exec),
            Some(PathBuf::from("/usr/bin/python"))
        );
        assert_eq!(SearchPath::new(None).resolve("python", is_exec), None);
        assert_eq!(path("/opt/env/bin").resolve("flask", is_exec), None);
        assert_eq!(path("/opt/env/bin").resolve("", |_| true), None);
    }

    #[test]
    fn names_with_a_slash_bypass_path() {
        assert_eq!(
            SearchPath::new(Some("/bin")).resolve("./run.sh", |_| false),
            Some(PathBuf::from("./run.sh"))
        );
        assert_eq!(
            SearchPath::new(None).resolve("/bin/sh", |_| false),
            Some(PathBuf::from("/bin/sh"))
        );
    }

    #[test]
    fn overlay_lowers_put_the_top_layer_first_and_boot_last() {
        let layers: Vec<PathBuf> = ["/fervor/l/0", "/fervor/l/1", "/fervor/l/2"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let lowers = OverlayLowers::new(&layers, Path::new("/fervor/boot"));
        let expected: Vec<PathBuf> = ["/fervor/l/2", "/fervor/l/1", "/fervor/l/0", "/fervor/boot"]
            .iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(lowers.as_slice(), expected);
        assert_eq!(
            OverlayLowers::new(&[], Path::new("/fervor/boot")).as_slice(),
            [PathBuf::from("/fervor/boot")]
        );
    }

    #[test]
    fn loop_range_covers_the_whole_aligned_slot() {
        let limit = |len| {
            LoopRange::of(&PackEntry {
                digest: [0; 32],
                offset: PACK_ALIGN,
                len,
            })
            .size_limit
        };
        assert_eq!(limit(1), PACK_ALIGN);
        assert_eq!(limit(PACK_ALIGN), PACK_ALIGN);
        assert_eq!(limit(PACK_ALIGN + 1), 2 * PACK_ALIGN);
    }
}
