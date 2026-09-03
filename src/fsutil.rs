use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use sha2::{Digest as _, Sha256};

use crate::{Digest, Error, Result, error::IoContext};

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn owner_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

pub(crate) fn hash_tree(root: &Path) -> Result<Digest> {
    let mut paths = Vec::new();
    collect_paths(root, root, &mut paths)?;
    paths.sort_by_key(|left| path_bytes(left));

    let mut hasher = Sha256::new();
    for relative in paths {
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path).at(&path)?;
        let encoded = path_bytes(&relative);
        hasher.update((encoded.len() as u64).to_le_bytes());
        hasher.update(&encoded);
        if metadata.file_type().is_symlink() {
            hasher.update(b"L");
            let target = fs::read_link(&path).at(&path)?;
            let target = path_bytes(&target);
            hasher.update((target.len() as u64).to_le_bytes());
            hasher.update(target);
        } else if metadata.is_dir() {
            hasher.update(b"D");
        } else if metadata.is_file() {
            hasher.update(b"F");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hasher.update((metadata.permissions().mode() & 0o777).to_le_bytes());
            }
            let mut file = File::open(&path).at(&path)?;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).at(&path)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
        } else {
            return Err(Error::InvalidArchive(format!(
                "unsupported filesystem entry {}",
                path.display()
            )));
        }
    }
    Ok(Digest::from_bytes(hasher.finalize().into()))
}

fn collect_paths(root: &Path, directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory).at(directory)? {
        let entry = entry.at(directory)?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|error| Error::InvalidArchive(error.to_string()))?
            .to_owned();
        output.push(relative);
        if entry.file_type().at(&path)?.is_dir() {
            collect_paths(root, &path, output)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination).at(destination)?;
    for entry in fs::read_dir(source).at(source)? {
        let entry = entry.at(source)?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type().at(&source_path)?;
        if file_type.is_dir() {
            copy_tree(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).at(&destination_path)?;
            let permissions = fs::metadata(&source_path).at(&source_path)?.permissions();
            fs::set_permissions(&destination_path, permissions).at(&destination_path)?;
        } else if file_type.is_symlink() {
            copy_symlink(&source_path, &destination_path)?;
        } else {
            return Err(Error::InvalidArchive(format!(
                "cannot copy special entry {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

pub(crate) fn copy_and_hash_file(source: &Path, destination: &Path) -> Result<Digest> {
    let mut input = File::open(source).at(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .at(destination)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer).at(source)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count]).at(destination)?;
        hasher.update(&buffer[..count]);
    }
    output.sync_all().at(destination)?;
    Ok(Digest::from_bytes(hasher.finalize().into()))
}

pub(crate) fn validate_tree_structure(root: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(root).at(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::InvalidArchive(format!(
            "package root {} is not a real directory",
            root.display()
        )));
    }
    let canonical_root = root.canonicalize().at(root)?;
    validate_entries(root, &canonical_root)
}

fn validate_entries(directory: &Path, canonical_root: &Path) -> Result<()> {
    for entry in fs::read_dir(directory).at(directory)? {
        let entry = entry.at(directory)?;
        let path = entry.path();
        let file_type = entry.file_type().at(&path)?;
        if file_type.is_dir() {
            validate_entries(&path, canonical_root)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&path).at(&path)?;
            if target.is_absolute() {
                return Err(Error::InvalidArchive(format!(
                    "absolute symlink target at {}",
                    path.display()
                )));
            }
            let resolved = path
                .parent()
                .expect("tree entry has a parent")
                .join(target)
                .canonicalize()
                .at(&path)?;
            if !resolved.starts_with(canonical_root) {
                return Err(Error::InvalidArchive(format!(
                    "symlink {} escapes the package root",
                    path.display()
                )));
            }
        } else if !file_type.is_file() {
            return Err(Error::InvalidArchive(format!(
                "special filesystem entry at {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).at(source)?;
    std::os::unix::fs::symlink(target, destination).at(destination)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source).at(source)?;
    let metadata = fs::metadata(source).at(source)?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination).at(destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination).at(destination)
    }
}

pub(crate) fn rename_with_retry(
    source: &Path,
    destination: &Path,
    timeout: Duration,
) -> std::io::Result<()> {
    retry_windows(timeout, || platform_rename(source, destination))?;
    sync_parent(destination)
}

#[cfg(not(windows))]
fn platform_rename(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn platform_rename(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{iter::once, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    // SAFETY: Both pointers reference NUL-terminated UTF-16 buffers for the duration of the call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn remove_with_retry(path: &Path, timeout: Duration) -> std::io::Result<()> {
    retry_windows(timeout, || match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
        }
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    })
}

fn retry_windows(
    timeout: Duration,
    mut operation: impl FnMut() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let started = Instant::now();
    let mut delay = Duration::from_millis(50);
    loop {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error)
                if cfg!(windows)
                    && transient_windows_error(&error)
                    && started.elapsed() < timeout =>
            {
                std::thread::sleep(delay.min(timeout.saturating_sub(started.elapsed())));
                delay = (delay * 2).min(Duration::from_millis(500));
            }
            Err(error) => return Err(error),
        }
    }
}

fn transient_windows_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33 | 145))
}

pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| Error::Io {
        path: path.to_owned(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    let temporary = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(OsStr::to_str).unwrap_or("state"),
        owner_token()
    ));
    let mut file = File::create(&temporary).at(&temporary)?;
    file.write_all(contents).at(&temporary)?;
    file.sync_all().at(&temporary)?;
    drop(file);
    platform_rename(&temporary, path).at(path)?;
    sync_parent(path).at(path)
}

fn sync_parent(_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = _path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
