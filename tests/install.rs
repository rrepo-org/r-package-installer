use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Barrier},
    thread,
};

use flate2::{Compression, write::GzEncoder};
use r_package_installer::{
    Artifact, BinaryArtifact, CacheKey, Digest, ExpectedPackage, InstallOutcome, Installer,
    InstallerOptions, LibraryEntry, PrepareRequest, RecoveryAction, RecoveryPlan, RemovalOutcome,
    SourceArtifact, SourceOptions, recover_package, scan_library,
};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use zip::{ZipWriter, write::SimpleFileOptions};

fn digest(path: &Path) -> Digest {
    Digest::from_bytes(Sha256::digest(fs::read(path).unwrap()).into())
}

fn expected(name: &str, version: &str) -> ExpectedPackage {
    ExpectedPackage {
        name: name.into(),
        version: version.into(),
        r_major_minor: None,
        platform: None,
        architecture: None,
    }
}

fn binary_zip(directory: &Path, name: &str, version: &str, payload: &str) -> PathBuf {
    let path = directory.join(format!("{name}-{version}.zip"));
    let mut zip = ZipWriter::new(fs::File::create(&path).unwrap());
    let options = SimpleFileOptions::default();
    zip.start_file(format!("{name}/DESCRIPTION"), options)
        .unwrap();
    write!(
        zip,
        "Package: {name}\nVersion: {version}\nBuilt: R 4.5.2; test-platform; now; unix\n"
    )
    .unwrap();
    zip.start_file(format!("{name}/Meta/package.rds"), options)
        .unwrap();
    zip.write_all(b"fake rds").unwrap();
    zip.start_file(format!("{name}/R/value"), options).unwrap();
    zip.write_all(payload.as_bytes()).unwrap();
    zip.finish().unwrap();
    path
}

fn binary_request(path: PathBuf, name: &str, version: &str) -> PrepareRequest {
    let digest = digest(&path);
    PrepareRequest {
        key: CacheKey::from_digest(digest),
        artifact_digest: digest,
        expected: expected(name, version),
        artifact: Artifact::Binary(BinaryArtifact { path }),
    }
}

fn package_tree(path: &Path, name: &str, version: &str, payload: &str) {
    fs::create_dir_all(path.join("Meta")).unwrap();
    fs::create_dir_all(path.join("R")).unwrap();
    fs::write(
        path.join("DESCRIPTION"),
        format!("Package: {name}\nVersion: {version}\nBuilt: R 4.5.2; test-platform; now; unix\n"),
    )
    .unwrap();
    fs::write(path.join("Meta/package.rds"), b"fake rds").unwrap();
    fs::write(path.join("R/value"), payload).unwrap();
}

fn binary_tar_bytes(escaping_link: bool) -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    append(
        &mut tar,
        "foo/DESCRIPTION",
        b"Package: foo\nVersion: 1.0\nBuilt: R 4.6.1; test-platform; now; unix\n",
    );
    append(&mut tar, "foo/Meta/package.rds", b"fake rds");
    append(&mut tar, "foo/R/value", b"payload");
    if escaping_link {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        tar.append_link(&mut header, "foo/escape", "../../outside")
            .unwrap();
    }
    tar.into_inner().unwrap()
}

fn compress_tar(bytes: &[u8], xz: bool) -> Vec<u8> {
    if xz {
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    } else {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
}

#[test]
fn detects_binary_contents_independently_of_filename() {
    for xz in [false, true] {
        for filename in ["foo.tgz", "foo.zip", "download"] {
            let temp = TempDir::new().unwrap();
            let path = temp.path().join(filename);
            fs::write(&path, compress_tar(&binary_tar_bytes(false), xz)).unwrap();
            let installer = Installer::new(temp.path().join("cache"));
            let library = temp.path().join("library");
            installer
                .install(&binary_request(path, "foo", "1.0"), &library)
                .unwrap();
            assert_eq!(fs::read(library.join("foo/R/value")).unwrap(), b"payload");
        }
    }
    let temp = TempDir::new().unwrap();
    let zip = binary_zip(temp.path(), "foo", "1.0", "payload");
    let path = temp.path().join("download.tgz");
    fs::rename(zip, &path).unwrap();
    Installer::new(temp.path().join("cache"))
        .prepare(&binary_request(path, "foo", "1.0"))
        .unwrap();
}

#[test]
fn rejects_corrupt_compression_and_preserves_installed_package() {
    for xz in [false, true] {
        let good = compress_tar(&binary_tar_bytes(false), xz);
        let mut corrupt = good.clone();
        let index = corrupt.len() - if xz { 9 } else { 8 };
        corrupt[index] ^= 0xff;
        for bytes in [
            good[..good.len() - 1].to_vec(),
            good[..good.len() / 2].to_vec(),
            corrupt,
        ] {
            let temp = TempDir::new().unwrap();
            let installer = Installer::new(temp.path().join("cache"));
            let library = temp.path().join("library");
            let old = binary_zip(temp.path(), "foo", "0.9", "old");
            installer
                .install(&binary_request(old, "foo", "0.9"), &library)
                .unwrap();
            let path = temp.path().join("bad.tgz");
            fs::write(&path, bytes).unwrap();
            let request = binary_request(path.clone(), "foo", "1.0");
            assert!(matches!(
                installer.install(&request, &library),
                Err(r_package_installer::Error::InvalidArchive(_))
            ));
            assert_eq!(fs::read(library.join("foo/R/value")).unwrap(), b"old");
            // Retry the same key with valid bytes to prove failed preparation left no lock/object.
            fs::write(&path, &good).unwrap();
            let mut retry = binary_request(path, "foo", "1.0");
            retry.key = request.key;
            installer.install(&retry, &library).unwrap();
        }
    }
}

#[test]
fn detected_tar_enforces_links_limits_and_identity() {
    for xz in [false, true] {
        for case in ["link", "limit", "identity", "version"] {
            let temp = TempDir::new().unwrap();
            let path = temp.path().join("download");
            fs::write(&path, compress_tar(&binary_tar_bytes(case == "link"), xz)).unwrap();
            let mut options = InstallerOptions::default();
            if case == "limit" {
                options.archive_limits.max_entries = 1;
            }
            let request = binary_request(
                path,
                if case == "identity" { "other" } else { "foo" },
                if case == "version" { "2.0" } else { "1.0" },
            );
            assert!(
                Installer::with_options(temp.path().join("cache"), options)
                    .prepare(&request)
                    .is_err()
            );
        }
    }
}

#[test]
fn distinguishes_unsupported_and_truncated_signatures() {
    for bytes in [b"BZh91".as_slice(), b"\x28\xb5\x2f\xfd", b"not an archive"] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("download");
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            Installer::new(temp.path().join("cache")).prepare(&binary_request(path, "foo", "1.0")),
            Err(r_package_installer::Error::UnsupportedArchiveFormat { .. })
        ));
    }
    for bytes in [
        b"".as_slice(),
        b"\xfd7z",
        b"\x1f",
        b"PK\x03",
        b"PK\x03\x04broken",
    ] {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("download");
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            Installer::new(temp.path().join("cache")).prepare(&binary_request(path, "foo", "1.0")),
            Err(r_package_installer::Error::InvalidArchive(_))
        ));
    }
}

#[test]
fn compression_trailer_validation_remains_bounded() {
    for xz in [false, true] {
        let temp = TempDir::new().unwrap();
        let mut tar = binary_tar_bytes(false);
        // Padding after the tar end marker must still count against the read limit.
        tar.resize(2 * 1024 * 1024, 0);
        let path = temp.path().join("download");
        fs::write(&path, compress_tar(&tar, xz)).unwrap();
        let mut options = InstallerOptions::default();
        options.archive_limits.max_entries = 3;
        options.archive_limits.max_expanded_size = 1024;
        let error = Installer::with_options(temp.path().join("cache"), options)
            .prepare(&binary_request(path, "foo", "1.0"))
            .unwrap_err();
        assert!(
            matches!(error, r_package_installer::Error::InvalidArchive(message) if message.contains("decompressed tar exceeds configured limit"))
        );
    }
}

fn recovery_lock(library: &Path, package: &str, state: &str) -> PathBuf {
    let lock = library.join(format!("00LOCK-{package}"));
    fs::create_dir_all(&lock).unwrap();
    fs::write(
        lock.join("owner.dcf"),
        format!(
            "Format: 1\nPackage: {package}\nPid: 999999\nToken: test-token-{package}\nOperation: install\n"
        ),
    )
    .unwrap();
    fs::write(
        lock.join("state-001.dcf"),
        format!("Format: 1\nPackage: {package}\nToken: test-token-{package}\nState: {state}\n"),
    )
    .unwrap();
    lock
}

fn recovery_plan(library: &Path, package: &str) -> RecoveryPlan {
    scan_library(library)
        .unwrap()
        .into_iter()
        .find_map(|entry| match entry {
            LibraryEntry::Recoverable(plan) if plan.lock.package.as_deref() == Some(package) => {
                Some(plan)
            }
            _ => None,
        })
        .unwrap()
}

#[test]
fn caches_installs_replaces_scans_and_removes() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("cache");
    let library = temp.path().join("library");
    let installer = Installer::new(&cache);

    let first = binary_request(
        binary_zip(temp.path(), "sample", "1.0", "one"),
        "sample",
        "1.0",
    );
    assert!(matches!(
        installer.install(&first, &library).unwrap(),
        InstallOutcome::Installed(_)
    ));
    assert_eq!(
        fs::read_to_string(library.join("sample/R/value")).unwrap(),
        "one"
    );
    assert!(matches!(
        installer.install(&first, &library).unwrap(),
        InstallOutcome::AlreadyInstalled(_)
    ));

    let second = binary_request(
        binary_zip(temp.path(), "sample", "2.0", "two"),
        "sample",
        "2.0",
    );
    assert!(matches!(
        installer.install(&second, &library).unwrap(),
        InstallOutcome::Replaced(_)
    ));
    assert_eq!(
        fs::read_to_string(library.join("sample/R/value")).unwrap(),
        "two"
    );
    assert!(!library.join("00LOCK-sample").exists());

    let entries = scan_library(&library).unwrap();
    assert!(
        matches!(entries.as_slice(), [LibraryEntry::Installed(package)] if package.metadata.version == "2.0")
    );
    assert_eq!(
        installer.remove(&library, "sample").unwrap(),
        RemovalOutcome::Removed
    );
    assert!(!library.join("sample").exists());
}

#[test]
fn concurrent_prepare_calls_share_one_cache_entry() {
    let temp = TempDir::new().unwrap();
    let request = Arc::new(binary_request(
        binary_zip(temp.path(), "parallel", "1.0", "value"),
        "parallel",
        "1.0",
    ));
    let installer = Arc::new(Installer::new(temp.path().join("cache")));
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let request = Arc::clone(&request);
        let installer = Arc::clone(&installer);
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            installer.prepare(&request).unwrap()
        }));
    }
    barrier.wait();
    let left = workers.remove(0).join().unwrap();
    let right = workers.remove(0).join().unwrap();
    assert_eq!(left.root, right.root);
    assert_eq!(left.tree_digest, right.tree_digest);
}

#[test]
fn foreign_r_package_lock_is_never_removed() {
    let temp = TempDir::new().unwrap();
    let cache = temp.path().join("cache");
    let library = temp.path().join("library");
    fs::create_dir_all(library.join("00LOCK-locked")).unwrap();
    let request = binary_request(
        binary_zip(temp.path(), "locked", "1.0", "value"),
        "locked",
        "1.0",
    );
    let error = Installer::new(cache)
        .install(&request, &library)
        .unwrap_err();
    assert!(error.to_string().contains("package locked is locked"));
    assert!(library.join("00LOCK-locked").exists());
}

#[test]
fn recovers_every_journaled_crash_state() {
    let temp = TempDir::new().unwrap();
    let options = InstallerOptions::default();

    let restore = temp.path().join("restore");
    fs::create_dir(&restore).unwrap();
    let lock = recovery_lock(&restore, "restorepkg", "OldMoved");
    package_tree(&lock.join("restorepkg"), "restorepkg", "1.0", "old");
    let plan = recovery_plan(&restore, "restorepkg");
    assert_eq!(plan.action, RecoveryAction::RestoreBackup);
    recover_package(&plan, &options).unwrap();
    assert_eq!(
        fs::read_to_string(restore.join("restorepkg/R/value")).unwrap(),
        "old"
    );
    assert!(!lock.exists());

    let rollback = temp.path().join("rollback");
    fs::create_dir(&rollback).unwrap();
    let lock = recovery_lock(&rollback, "rollbackpkg", "RollbackRequired");
    package_tree(&rollback.join("rollbackpkg"), "rollbackpkg", "2.0", "new");
    package_tree(&lock.join("rollbackpkg"), "rollbackpkg", "1.0", "old");
    let plan = recovery_plan(&rollback, "rollbackpkg");
    assert_eq!(plan.action, RecoveryAction::RollbackPublication);
    recover_package(&plan, &options).unwrap();
    assert_eq!(
        fs::read_to_string(rollback.join("rollbackpkg/R/value")).unwrap(),
        "old"
    );
    assert!(!lock.exists());

    let commit = temp.path().join("commit");
    fs::create_dir(&commit).unwrap();
    let lock = recovery_lock(&commit, "commitpkg", "Committed");
    package_tree(&commit.join("commitpkg"), "commitpkg", "2.0", "new");
    package_tree(&lock.join("commitpkg"), "commitpkg", "1.0", "old");
    let plan = recovery_plan(&commit, "commitpkg");
    assert_eq!(plan.action, RecoveryAction::FinishCommit);
    recover_package(&plan, &options).unwrap();
    assert_eq!(
        fs::read_to_string(commit.join("commitpkg/R/value")).unwrap(),
        "new"
    );
    assert!(!lock.exists());

    let staging = temp.path().join("staging");
    fs::create_dir(&staging).unwrap();
    let lock = recovery_lock(&staging, "stagingpkg", "Prepared");
    package_tree(&lock.join("00new/stagingpkg"), "stagingpkg", "1.0", "new");
    let plan = recovery_plan(&staging, "stagingpkg");
    assert_eq!(plan.action, RecoveryAction::RemoveStaging);
    recover_package(&plan, &options).unwrap();
    assert!(!lock.exists());

    let removal = temp.path().join("removal");
    fs::create_dir(&removal).unwrap();
    let lock = recovery_lock(&removal, "removalpkg", "RemovalCommitted");
    package_tree(&lock.join("removalpkg"), "removalpkg", "1.0", "old");
    let plan = recovery_plan(&removal, "removalpkg");
    assert_eq!(plan.action, RecoveryAction::FinishRemoval);
    recover_package(&plan, &options).unwrap();
    assert!(!lock.exists());
    assert!(!removal.join("removalpkg").exists());
}

#[test]
fn recovery_rejects_a_stale_filesystem_plan() {
    let temp = TempDir::new().unwrap();
    let library = temp.path().join("library");
    fs::create_dir(&library).unwrap();
    let lock = recovery_lock(&library, "stalepkg", "OldMoved");
    package_tree(&lock.join("stalepkg"), "stalepkg", "1.0", "old");
    let plan = recovery_plan(&library, "stalepkg");

    package_tree(&library.join("stalepkg"), "stalepkg", "2.0", "new");
    let error = recover_package(&plan, &InstallerOptions::default()).unwrap_err();
    assert!(error.to_string().contains("filesystem state changed"));
    assert!(lock.exists());
    assert_eq!(
        fs::read_to_string(library.join("stalepkg/R/value")).unwrap(),
        "new"
    );
}

fn source_tarball(directory: &Path) -> PathBuf {
    let path = directory.join("sourcepkg_1.0.tar.gz");
    let encoder = GzEncoder::new(fs::File::create(&path).unwrap(), Compression::fast());
    let mut tar = tar::Builder::new(encoder);
    append(&mut tar, "sourcepkg/DESCRIPTION", b"Package: sourcepkg\nVersion: 1.0\nTitle: Source package\nDescription: Integration test.\nLicense: MIT\nAuthor: Test Person\nMaintainer: Test Person <test@example.com>\nEncoding: UTF-8\n");
    append(&mut tar, "sourcepkg/NAMESPACE", b"export(answer)\n");
    append(
        &mut tar,
        "sourcepkg/R/answer.R",
        b"answer <- function() 42\n",
    );
    tar.into_inner().unwrap().finish().unwrap();
    path
}

fn append<W: Write>(tar: &mut tar::Builder<W>, path: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_path(path).unwrap();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    tar.append(&header, bytes).unwrap();
}

#[test]
fn source_is_materialized_by_r_when_available() {
    if Command::new("R").arg("--version").output().is_err() {
        return;
    }
    let temp = TempDir::new().unwrap();
    let path = source_tarball(temp.path());
    let artifact_digest = digest(&path);
    let request = PrepareRequest {
        key: CacheKey::from_digest(Digest::from_bytes(
            Sha256::digest(b"source-build-context").into(),
        )),
        artifact_digest,
        expected: expected("sourcepkg", "1.0"),
        artifact: Artifact::Source(SourceArtifact {
            path,
            options: SourceOptions::default(),
        }),
    };
    let installer = Installer::with_options(temp.path().join("cache"), InstallerOptions::default());
    let entry = installer.prepare(&request).unwrap();
    assert!(entry.package_root.join("Meta/package.rds").is_file());
    assert!(!entry.package.built.is_empty());
}
