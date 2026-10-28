//! The system links layer: `/bin/sh`, `/bin/bash` and `/usr/bin/env` as
//! absolute symlinks into the environment, for programs that hard-code
//! these paths (`#!/bin/sh`, `#!/usr/bin/env python`, root's login shell).

use std::collections::BTreeSet;

use crate::digest::LayerKey;
use crate::files::{EntryKind, FileMode, FileOwner, FileSet, LayerEntry};
use crate::layer::{EnvPrefix, LayerIndex, LayerKeyInput, LayoutVersion};
use crate::nonempty::NonEmpty;
use crate::path::GuestPath;

/// Each link path with its candidate targets in `<prefix>/`, by preference.
const CANDIDATES: &[(&str, &[&str])] = &[
    ("/bin/bash", &["bin/bash"]),
    ("/bin/sh", &["bin/sh", "bin/dash", "bin/bash", "bin/busybox"]),
    ("/usr/bin/env", &["bin/env"]),
];

/// An absolute symlink `path → target`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SystemLink {
    pub path: GuestPath,
    pub target: GuestPath,
}

/// The links that apply to one stack, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemLinks(NonEmpty<SystemLink>);

impl SystemLinks {
    /// Decides the links from the stack's layer `indices`: a link applies
    /// when one of its targets exists in `prefix` (the first one wins) and
    /// no layer provides the link path itself. `None` when no link applies.
    pub fn for_stack<'a>(prefix: &EnvPrefix, indices: impl IntoIterator<Item = &'a LayerIndex>) -> Option<Self> {
        let present: BTreeSet<&GuestPath> = indices.into_iter().flat_map(|index| &index.entries).map(|entry| &entry.path).collect();
        let links = CANDIDATES
            .iter()
            .filter_map(|(path, targets)| {
                let path = GuestPath::new(*path).expect("valid link path");
                if present.contains(&path) {
                    return None;
                }
                let target = targets
                    .iter()
                    .map(|target| prefix.path().join(target).expect("valid relative path"))
                    .find(|target| present.contains(target))?;
                Some(SystemLink { path, target })
            })
            .collect();
        NonEmpty::new(links).map(Self)
    }

    pub fn links(&self) -> &[SystemLink] {
        &self.0
    }

    pub fn key(&self, layout: LayoutVersion) -> LayerKey {
        LayerKey::derive(layout, LayerKeyInput::SystemLinks { links: self.links() })
    }

    pub fn label(&self) -> String {
        let paths: Vec<&str> = self.links().iter().map(|link| link.path.as_str()).collect();
        format!("system links ({})", paths.join(", "))
    }

    /// The links plus their parent directories (`0755`).
    pub fn files(&self) -> FileSet {
        let dirs: BTreeSet<GuestPath> = self
            .links()
            .iter()
            .flat_map(|link| std::iter::successors(link.path.parent(), GuestPath::parent))
            .filter(|dir| !dir.is_root())
            .collect();
        let entries = dirs
            .into_iter()
            .map(|path| LayerEntry { path, kind: EntryKind::Directory { mode: FileMode::DIRECTORY } })
            .chain(self.links().iter().map(|link| LayerEntry {
                path: link.path.clone(),
                kind: EntryKind::Symlink { target: link.target.to_string() },
            }))
            .collect();
        FileSet { owner: FileOwner::SystemLinks, entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::IndexEntry;

    fn index(paths: &[&str]) -> LayerIndex {
        LayerIndex {
            entries: paths
                .iter()
                .map(|path| IndexEntry { path: GuestPath::new(*path).unwrap(), owner: FileOwner::Package("p".into()) })
                .collect(),
        }
    }

    fn links(indices: &[LayerIndex]) -> Vec<(String, String)> {
        SystemLinks::for_stack(&EnvPrefix::default(), indices)
            .map(|links| links.links().iter().map(|l| (l.path.to_string(), l.target.to_string())).collect())
            .unwrap_or_default()
    }

    fn pair(path: &str, target: &str) -> (String, String) {
        (path.to_owned(), target.to_owned())
    }

    #[test]
    fn bash_and_coreutils_provide_every_link() {
        let stack = [index(&["/opt/env/bin/bash"]), index(&["/opt/env/bin/env", "/opt/env/bin/ls"])];
        assert_eq!(
            links(&stack),
            [
                pair("/bin/bash", "/opt/env/bin/bash"),
                pair("/bin/sh", "/opt/env/bin/bash"),
                pair("/usr/bin/env", "/opt/env/bin/env"),
            ]
        );
    }

    #[test]
    fn sh_prefers_sh_then_dash_then_bash_then_busybox() {
        let all = ["/opt/env/bin/busybox", "/opt/env/bin/bash", "/opt/env/bin/dash", "/opt/env/bin/sh"];
        for skip in 0..all.len() {
            let stack = [index(&all[..all.len() - skip])];
            let sh = links(&stack).into_iter().find(|(path, _)| path == "/bin/sh").unwrap();
            assert_eq!(sh.1, all[all.len() - skip - 1]);
        }
    }

    #[test]
    fn absent_targets_and_provided_paths_add_no_links() {
        assert!(SystemLinks::for_stack(&EnvPrefix::default(), &[index(&["/opt/env/bin/python", "/bin/env"])]).is_none());
        assert!(SystemLinks::for_stack(&EnvPrefix::default(), &[] as &[LayerIndex]).is_none());
        // A path a layer already provides is left alone.
        let stack = [index(&["/opt/env/bin/bash"]), index(&["/bin/sh"])];
        assert_eq!(links(&stack), [pair("/bin/bash", "/opt/env/bin/bash")]);
    }

    #[test]
    fn layer_holds_links_and_parent_directories_keyed_by_the_link_set() {
        let full = SystemLinks::for_stack(&EnvPrefix::default(), &[index(&["/opt/env/bin/bash", "/opt/env/bin/env"])]).unwrap();
        let files = full.files();
        assert_eq!(files.owner, FileOwner::SystemLinks);
        let entries: Vec<(&str, &EntryKind)> = files.entries.iter().map(|e| (e.path.as_str(), &e.kind)).collect();
        let dir = EntryKind::Directory { mode: FileMode::DIRECTORY };
        let link = |target: &str| EntryKind::Symlink { target: target.to_owned() };
        assert_eq!(
            entries,
            [
                ("/bin", &dir),
                ("/usr", &dir),
                ("/usr/bin", &dir),
                ("/bin/bash", &link("/opt/env/bin/bash")),
                ("/bin/sh", &link("/opt/env/bin/bash")),
                ("/usr/bin/env", &link("/opt/env/bin/env")),
            ]
        );

        let layout = LayoutVersion::CURRENT;
        let bash_only = SystemLinks::for_stack(&EnvPrefix::default(), &[index(&["/opt/env/bin/bash"])]).unwrap();
        let dash = SystemLinks::for_stack(&EnvPrefix::default(), &[index(&["/opt/env/bin/bash", "/opt/env/bin/dash"])]).unwrap();
        assert_eq!(full.key(layout), full.clone().key(layout));
        assert_ne!(full.key(layout), bash_only.key(layout));
        assert_ne!(bash_only.key(layout), dash.key(layout));
        assert_ne!(full.key(layout), full.key(LayoutVersion(2)));
    }
}
