use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use camino::{Utf8Path, Utf8PathBuf};
use fervor_domain::environment::{PythonAbi, ResolvedPackage};
use fervor_domain::files::{EntryKind, FileContent, FileSet};
use fervor_domain::layer::EnvPrefix;
use fervor_domain::platform::GuestPlatform;
use rattler_conda_types::compression_level::CompressionLevel;
use rattler_conda_types::package::DistArchiveIdentifier;
use rattler_conda_types::prefix_record::PrefixRecord;
use rattler_conda_types::{NoArchType, PackageName, PackageRecord, RepoDataRecord, Version};
use rattler_digest::{Sha256, compute_bytes_digest};
use serde_json::json;

use crate::install::InstallTarget;
use crate::{ContentsError, RattlerPackageContents};
use crate::archive::PackageArchive;
use crate::install::{InstallError, PackageInstall};

const PLACEHOLDER: &str = "/build/placehold_placehold_placehold_placehold";

enum Node {
    File(&'static [u8], u32),
    Symlink(&'static str),
    Dir,
}

/// Writes `nodes` into a package directory and packs it as `file_name`
/// (`.conda` or `.tar.bz2`) inside `out`.
fn pack(work: &Path, nodes: &[(&str, Node)], file_name: &str, out: &Path) -> PathBuf {
    let base = work.join(file_name.replace('.', "_"));
    let mut paths = Vec::new();
    for (path, node) in nodes {
        let full = base.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        match node {
            Node::File(bytes, mode) => {
                fs::write(&full, bytes).unwrap();
                fs::set_permissions(&full, fs::Permissions::from_mode(*mode)).unwrap();
            }
            Node::Symlink(target) => std::os::unix::fs::symlink(target, &full).unwrap(),
            Node::Dir => fs::create_dir_all(&full).unwrap(),
        }
        paths.push(full);
    }
    fs::create_dir_all(out).unwrap();
    let archive = out.join(file_name);
    let file = fs::File::create(&archive).unwrap();
    if let Some(stem) = file_name.strip_suffix(".conda") {
        rattler_package_streaming::write::write_conda_package(
            file,
            &base,
            &paths,
            CompressionLevel::Lowest,
            Some(1),
            stem,
            None,
            None,
        )
        .unwrap();
    } else {
        rattler_package_streaming::write::write_tar_bz2_package(file, &base, &paths, CompressionLevel::Lowest, None, None)
            .unwrap();
    }
    archive
}

fn record(name: &str, file: &Path, noarch: NoArchType) -> ResolvedPackage {
    let bytes = fs::read(file).unwrap();
    let mut package_record = PackageRecord::new(
        PackageName::new_unchecked(name),
        Version::from_str("1.0").unwrap(),
        "0".to_owned(),
    );
    package_record.sha256 = Some(compute_bytes_digest::<Sha256>(&bytes));
    package_record.size = Some(bytes.len() as u64);
    package_record.noarch = noarch;
    let file_name = file.file_name().unwrap().to_str().unwrap();
    ResolvedPackage::new(RepoDataRecord {
        package_record,
        identifier: DistArchiveIdentifier::try_from_filename(file_name).unwrap(),
        url: url::Url::from_file_path(file).unwrap(),
        channel: None,
    })
    .unwrap()
}

fn target(python: Option<PythonAbi>) -> InstallTarget {
    InstallTarget { prefix: EnvPrefix::default(), platform: GuestPlatform::LinuxAarch64, python }
}

const TOOL: &[u8] = b"#!/build/placehold_placehold_placehold_placehold/bin/sh\necho /build/placehold_placehold_placehold_placehold/share\n";
const LIB: &[u8] = b"\x7fELF\0/build/placehold_placehold_placehold_placehold/lib\0tail";

fn demo_nodes(paths_json: &'static [u8]) -> Vec<(&'static str, Node)> {
    vec![
        ("info/paths.json", Node::File(paths_json, 0o644)),
        ("bin/tool", Node::File(TOOL, 0o755)),
        ("lib/libdemo.so.1", Node::File(LIB, 0o644)),
        ("lib/libdemo.so", Node::Symlink("libdemo.so.1")),
        ("share/empty", Node::Dir),
    ]
}

fn demo_paths_json() -> &'static [u8] {
    let paths = json!({
        "paths_version": 1,
        "paths": [
            { "_path": "bin/tool", "path_type": "hardlink", "prefix_placeholder": PLACEHOLDER, "file_mode": "text" },
            { "_path": "lib/libdemo.so.1", "path_type": "hardlink", "prefix_placeholder": PLACEHOLDER, "file_mode": "binary" },
            { "_path": "lib/libdemo.so", "path_type": "softlink" },
            { "_path": "share/empty", "path_type": "directory" },
        ]
    });
    Vec::leak(serde_json::to_vec(&paths).unwrap())
}

fn file_bytes<'a>(set: &'a FileSet, path: &str) -> (&'a [u8], u16) {
    let entry = set.entries.iter().find(|e| e.path.as_str() == path).unwrap_or_else(|| panic!("{path} missing"));
    match &entry.kind {
        EntryKind::File { content: FileContent::Bytes(bytes), mode } => (bytes, mode.bits()),
        other => panic!("{path} is {other:?}"),
    }
}

#[test]
fn conda_and_tar_bz2_decode_to_the_same_entries() {
    let work = tempfile::tempdir().unwrap();
    let nodes = demo_nodes(demo_paths_json());
    let conda = pack(work.path(), &nodes, "demo-1.0-0.conda", &work.path().join("out"));
    let bz2 = pack(work.path(), &nodes, "demo-1.0-0.tar.bz2", &work.path().join("out"));

    let conda = PackageArchive::read(Utf8Path::from_path(&conda).unwrap()).unwrap();
    let bz2 = PackageArchive::read(Utf8Path::from_path(&bz2).unwrap()).unwrap();
    assert_eq!(conda.content, bz2.content);
    assert_eq!(conda.info, bz2.info);

    assert!(conda.info.contains_key("info/paths.json"));
    assert!(conda.content.keys().all(|path| !path.starts_with("info")));
    assert_eq!(
        conda.content["bin/tool"],
        EntryKind::File { content: FileContent::Bytes(TOOL.to_vec()), mode: fervor_domain::files::FileMode::new(0o755) }
    );
    assert_eq!(conda.content["lib/libdemo.so"], EntryKind::Symlink { target: "libdemo.so.1".to_owned() });
    assert!(matches!(conda.content["share/empty"], EntryKind::Directory { .. }));
}

#[test]
fn installs_into_the_prefix_with_placeholders_replaced() {
    let work = tempfile::tempdir().unwrap();
    let file = pack(work.path(), &demo_nodes(demo_paths_json()), "demo-1.0-0.tar.bz2", &work.path().join("out"));
    let package = record("demo", &file, NoArchType::none());
    let archive = PackageArchive::read(Utf8Path::from_path(&file).unwrap()).unwrap();
    let set = PackageInstall::new(package.record(), archive, &target(None)).files().unwrap();

    assert!(set.entries.iter().all(|e| e.path.as_str().starts_with("/opt/env/")));

    let (tool, mode) = file_bytes(&set, "/opt/env/bin/tool");
    assert_eq!(tool, b"#!/opt/env/bin/sh\necho /opt/env/share\n");
    assert_eq!(mode, 0o755);

    // Binary placeholders are replaced as C strings: same length, NUL padded.
    let (lib, _) = file_bytes(&set, "/opt/env/lib/libdemo.so.1");
    assert_eq!(lib.len(), LIB.len());
    assert!(lib.starts_with(b"\x7fELF\0/opt/env/lib\0\0"));
    assert!(lib.ends_with(b"\0tail"));

    let link = set.entries.iter().find(|e| e.path.as_str() == "/opt/env/lib/libdemo.so").unwrap();
    assert_eq!(link.kind, EntryKind::Symlink { target: "libdemo.so.1".to_owned() });
    let dir = set.entries.iter().find(|e| e.path.as_str() == "/opt/env/share/empty").unwrap();
    assert!(matches!(dir.kind, EntryKind::Directory { .. }));

    let (meta, _) = file_bytes(&set, "/opt/env/conda-meta/demo-1.0-0.json");
    let meta = PrefixRecord::from_reader(meta).unwrap();
    let tool_meta = meta.paths_data.paths.iter().find(|p| p.relative_path == Path::new("bin/tool")).unwrap();
    assert_eq!(tool_meta.sha256_in_prefix, Some(compute_bytes_digest::<Sha256>(tool)));
    assert_eq!(meta.paths_data.paths.len(), 4);
}

#[test]
fn rejects_a_prefix_longer_than_a_binary_placeholder() {
    let work = tempfile::tempdir().unwrap();
    let file = pack(work.path(), &demo_nodes(demo_paths_json()), "demo-1.0-0.conda", &work.path().join("out"));
    let package = record("demo", &file, NoArchType::none());
    let archive = PackageArchive::read(Utf8Path::from_path(&file).unwrap()).unwrap();
    let mut long = target(None);
    long.prefix = EnvPrefix::new(fervor_domain::path::GuestPath::new(format!("/opt/{}", "x".repeat(80))).unwrap());
    let error = PackageInstall::new(package.record(), archive, &long).files().unwrap_err();
    assert!(matches!(error, InstallError::Placeholder { ref path, .. } if path == "lib/libdemo.so.1"), "{error:?}");
}

fn noarch_nodes() -> Vec<(&'static str, Node)> {
    let paths = json!({
        "paths_version": 1,
        "paths": [
            { "_path": "site-packages/app/__init__.py", "path_type": "hardlink" },
            { "_path": "python-scripts/helper", "path_type": "hardlink" },
        ]
    });
    let link = json!({
        "noarch": { "type": "python", "entry_points": ["app = app.cli:main"] },
        "package_metadata_version": 1
    });
    vec![
        ("info/paths.json", Node::File(Vec::leak(serde_json::to_vec(&paths).unwrap()), 0o644)),
        ("info/link.json", Node::File(Vec::leak(serde_json::to_vec(&link).unwrap()), 0o644)),
        ("site-packages/app/__init__.py", Node::File(b"", 0o644)),
        ("python-scripts/helper", Node::File(b"echo hi\n", 0o755)),
    ]
}

#[test]
fn noarch_python_is_relocated_and_gets_entry_points() {
    let work = tempfile::tempdir().unwrap();
    let file = pack(work.path(), &noarch_nodes(), "app-1.0-0.conda", &work.path().join("out"));
    let package = record("app", &file, NoArchType::python());
    let archive = PackageArchive::read(Utf8Path::from_path(&file).unwrap()).unwrap();
    let python = PythonAbi { major: 3, minor: 12 };
    let set = PackageInstall::new(package.record(), archive, &target(Some(python))).files().unwrap();

    file_bytes(&set, "/opt/env/lib/python3.12/site-packages/app/__init__.py");
    let (_, helper_mode) = file_bytes(&set, "/opt/env/bin/helper");
    assert_eq!(helper_mode, 0o755);
    let (entry_point, mode) = file_bytes(&set, "/opt/env/bin/app");
    assert_eq!(mode, 0o755);
    let script = std::str::from_utf8(entry_point).unwrap();
    assert!(script.starts_with("#!/opt/env/bin/python3.12\n"), "{script}");
    assert!(script.contains("from app.cli import main"), "{script}");

    let error = PackageInstall::new(
        package.record(),
        PackageArchive::read(Utf8Path::from_path(&file).unwrap()).unwrap(),
        &target(None),
    )
    .files()
    .unwrap_err();
    assert!(matches!(error, InstallError::MissingPython));
}

#[tokio::test]
async fn archives_are_verified_before_they_are_cached() {
    let work = tempfile::tempdir().unwrap();
    let file = pack(work.path(), &demo_nodes(demo_paths_json()), "demo-1.0-0.conda", &work.path().join("channel"));
    let root = Utf8PathBuf::from_path_buf(work.path().join("cache")).unwrap();
    let contents = RattlerPackageContents::new(&root, crate::CondaClient::authenticated().unwrap());

    let good = record("demo", &file, NoArchType::none());
    let mut tampered = good.record().clone();
    tampered.package_record.sha256 = Some(compute_bytes_digest::<Sha256>(b"something else"));
    let tampered = ResolvedPackage::new(tampered).unwrap();
    let error = contents.archive_entries(&tampered).await.unwrap_err();
    assert!(matches!(error, ContentsError::DigestMismatch { .. }), "{error:?}");
    let cached = |root: &Utf8Path| fs::read_dir(root.join("pkgs")).unwrap().count();
    assert_eq!(cached(&root), 0, "a mismatching archive must not stay in the cache");

    contents.archive_entries(&good).await.unwrap();
    assert_eq!(cached(&root), 1);
    // The verified copy is reused: the source can disappear.
    fs::remove_file(&file).unwrap();
    let entries = contents.archive_entries(&good).await.unwrap();
    assert!(entries.iter().any(|e| e.path == "lib/libdemo.so.1"));
}
