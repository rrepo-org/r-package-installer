mod tar_compat;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use flate2::read::GzDecoder;

use crate::{
    ArchiveLimits, BinaryArtifact, BinaryFormat, Error, ExpectedPackage, Result, error::IoContext,
};

use self::tar_compat::LinkSizeFix;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Directory,
    File,
    Symlink,
    Hardlink,
}

#[derive(Debug, Clone)]
struct ManifestEntry {
    path: PathBuf,
    kind: Kind,
    size: u64,
    mode: Option<u32>,
    target: Option<PathBuf>,
}

pub(crate) fn extract_binary(
    artifact: &BinaryArtifact,
    destination: &Path,
    expected: &ExpectedPackage,
    limits: &ArchiveLimits,
) -> Result<PathBuf> {
    fs::create_dir_all(destination).at(destination)?;
    match artifact.format {
        BinaryFormat::Zip => extract_zip(&artifact.path, destination, expected, limits)?,
        BinaryFormat::TarGz => extract_tar(&artifact.path, destination, expected, limits)?,
    }
    Ok(destination.join(&expected.name))
}

fn extract_zip(
    archive_path: &Path,
    destination: &Path,
    expected: &ExpectedPackage,
    limits: &ArchiveLimits,
) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(archive_path).at(archive_path)?)
        .map_err(|error| Error::InvalidArchive(error.to_string()))?;
    if archive.len() > limits.max_entries {
        return Err(Error::InvalidArchive(format!(
            "archive has more than {} entries",
            limits.max_entries
        )));
    }
    let mut manifest = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| Error::InvalidArchive(error.to_string()))?;
        let path = entry
            .enclosed_name()
            .ok_or_else(|| Error::InvalidArchive(format!("unsafe ZIP path {}", entry.name())))?
            .to_owned();
        let mode = entry.unix_mode();
        let kind = if entry.is_dir() {
            Kind::Directory
        } else if mode.is_some_and(|value| value & 0o170000 == 0o120000) {
            Kind::Symlink
        } else {
            Kind::File
        };
        let target = if kind == Kind::Symlink {
            if entry.size() > limits.max_path_bytes as u64 {
                return Err(Error::InvalidArchive(
                    "ZIP symlink target is too long".into(),
                ));
            }
            let mut value = String::new();
            entry
                .read_to_string(&mut value)
                .map_err(|error| Error::InvalidArchive(error.to_string()))?;
            Some(PathBuf::from(value))
        } else {
            None
        };
        manifest.push(ManifestEntry {
            path,
            kind,
            size: entry.size(),
            mode,
            target,
        });
    }
    validate_manifest(
        &manifest,
        expected,
        limits,
        fs::metadata(archive_path).at(archive_path)?.len(),
    )?;

    for (index, item) in manifest
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item.kind, Kind::Directory | Kind::File))
    {
        let output = destination.join(&item.path);
        if item.kind == Kind::Directory {
            fs::create_dir_all(&output).at(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        let mut input = archive
            .by_index(index)
            .map_err(|error| Error::InvalidArchive(error.to_string()))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .at(&output)?;
        std::io::copy(&mut input, &mut file).at(&output)?;
        file.flush().at(&output)?;
        set_mode(&output, item.mode)?;
    }
    materialize_links(destination, &manifest)
}

fn tar_archive(
    path: &Path,
    limits: &ArchiveLimits,
) -> Result<tar::Archive<LinkSizeFix<ReadLimit<GzDecoder<File>>>>> {
    let file = File::open(path).at(path)?;
    let metadata_allowance = (limits.max_entries as u64)
        .saturating_mul(1024)
        .saturating_add(1024 * 1024);
    Ok(tar::Archive::new(LinkSizeFix::new(ReadLimit::new(
        GzDecoder::new(file),
        limits.max_expanded_size.saturating_add(metadata_allowance),
    ))))
}

fn extract_tar(
    archive_path: &Path,
    destination: &Path,
    expected: &ExpectedPackage,
    limits: &ArchiveLimits,
) -> Result<()> {
    let mut archive = tar_archive(archive_path, limits)?;
    let mut manifest = Vec::new();
    for entry in archive
        .entries()
        .map_err(|error| Error::InvalidArchive(error.to_string()))?
    {
        let entry = entry.map_err(|error| Error::InvalidArchive(error.to_string()))?;
        if manifest.len() >= limits.max_entries {
            return Err(Error::InvalidArchive(format!(
                "archive has more than {} entries",
                limits.max_entries
            )));
        }
        let path = entry
            .path()
            .map_err(|error| Error::InvalidArchive(error.to_string()))?
            .into_owned();
        let entry_type = entry.header().entry_type();
        let kind = if entry_type.is_dir() {
            Kind::Directory
        } else if entry_type.is_file() {
            Kind::File
        } else if entry_type.is_symlink() {
            Kind::Symlink
        } else if entry_type.is_hard_link() {
            Kind::Hardlink
        } else {
            return Err(Error::InvalidArchive(format!(
                "unsupported tar entry type for {}",
                path.display()
            )));
        };
        let target = if matches!(kind, Kind::Symlink | Kind::Hardlink) {
            let target = entry
                .link_name()
                .map_err(|error| Error::InvalidArchive(error.to_string()))?
                .ok_or_else(|| Error::InvalidArchive("link has no target".into()))?
                .into_owned();
            validate_link_target(&target, limits)?;
            Some(target)
        } else {
            None
        };
        manifest.push(ManifestEntry {
            path,
            kind,
            size: entry.size(),
            mode: entry.header().mode().ok(),
            target,
        });
    }
    validate_manifest(
        &manifest,
        expected,
        limits,
        fs::metadata(archive_path).at(archive_path)?.len(),
    )?;

    let mut archive = tar_archive(archive_path, limits)?;
    for (entry, item) in archive
        .entries()
        .map_err(|error| Error::InvalidArchive(error.to_string()))?
        .zip(&manifest)
    {
        if !matches!(item.kind, Kind::Directory | Kind::File) {
            continue;
        }
        let mut entry = entry.map_err(|error| Error::InvalidArchive(error.to_string()))?;
        let output = destination.join(&item.path);
        if item.kind == Kind::Directory {
            fs::create_dir_all(&output).at(&output)?;
        } else {
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent).at(parent)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)
                .at(&output)?;
            std::io::copy(&mut entry, &mut file).at(&output)?;
            file.flush().at(&output)?;
            set_mode(&output, item.mode)?;
        }
    }
    materialize_links(destination, &manifest)
}

fn validate_manifest(
    manifest: &[ManifestEntry],
    expected: &ExpectedPackage,
    limits: &ArchiveLimits,
    compressed_size: u64,
) -> Result<()> {
    if manifest.is_empty() || manifest.len() > limits.max_entries {
        return Err(Error::InvalidArchive(format!(
            "archive has {} entries; limit is {}",
            manifest.len(),
            limits.max_entries
        )));
    }
    let mut paths = BTreeMap::new();
    let mut folded = BTreeSet::new();
    let mut roots = BTreeSet::new();
    let mut expanded = 0_u64;
    for entry in manifest {
        validate_relative_path(&entry.path, limits)?;
        let root = entry
            .path
            .components()
            .next()
            .expect("validated nonempty path");
        roots.insert(root.as_os_str().to_owned());
        if paths.insert(entry.path.clone(), entry.kind).is_some() {
            return Err(Error::InvalidArchive(format!(
                "duplicate path {}",
                entry.path.display()
            )));
        }
        let case_key = entry.path.to_string_lossy().to_ascii_lowercase();
        if !folded.insert(case_key) {
            return Err(Error::InvalidArchive(format!(
                "case-folded path collision at {}",
                entry.path.display()
            )));
        }
        if entry.size > limits.max_file_size {
            return Err(Error::InvalidArchive(format!(
                "entry {} is too large",
                entry.path.display()
            )));
        }
        expanded = expanded
            .checked_add(entry.size)
            .ok_or_else(|| Error::InvalidArchive("expanded size overflow".into()))?;
        if expanded > limits.max_expanded_size {
            return Err(Error::InvalidArchive(
                "expanded archive is too large".into(),
            ));
        }
    }
    if roots.len() != 1
        || roots.first().and_then(|value| value.to_str()) != Some(expected.name.as_str())
    {
        return Err(Error::InvalidArchive(format!(
            "archive must contain exactly the top-level directory {}",
            expected.name
        )));
    }
    if compressed_size != 0 && expanded > compressed_size.saturating_mul(limits.max_expansion_ratio)
    {
        return Err(Error::InvalidArchive(
            "archive expansion ratio exceeds limit".into(),
        ));
    }

    for entry in manifest {
        for ancestor in entry.path.ancestors().skip(1) {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            if paths
                .get(ancestor)
                .is_some_and(|kind| *kind != Kind::Directory)
            {
                return Err(Error::InvalidArchive(format!(
                    "{} traverses a non-directory entry",
                    entry.path.display()
                )));
            }
        }
        if let Some(target) = &entry.target {
            validate_link_target(target, limits)?;
            let resolved = match entry.kind {
                Kind::Symlink => {
                    resolve_link(entry.path.parent().unwrap_or(Path::new("")), target)?
                }
                Kind::Hardlink => resolve_link(Path::new(""), target)?,
                _ => continue,
            };
            if resolved
                .components()
                .next()
                .and_then(|part| part.as_os_str().to_str())
                != Some(expected.name.as_str())
            {
                return Err(Error::InvalidArchive(format!(
                    "link {} escapes the package root",
                    entry.path.display()
                )));
            }
            if entry.kind == Kind::Hardlink && paths.get(&resolved) != Some(&Kind::File) {
                return Err(Error::InvalidArchive(format!(
                    "hardlink {} does not target a regular file",
                    entry.path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_relative_path(path: &Path, limits: &ArchiveLimits) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::InvalidArchive("archive path is not UTF-8".into()))?;
    if text.is_empty() || text.len() > limits.max_path_bytes {
        return Err(Error::InvalidArchive(
            "archive path has invalid length".into(),
        ));
    }
    let components = path.components().collect::<Vec<_>>();
    if components.len() > limits.max_depth
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::InvalidArchive(format!("unsafe archive path {text}")));
    }
    for component in components {
        let value = component.as_os_str().to_string_lossy();
        if value.contains('\0') || value.contains('\\') || invalid_windows_component(&value) {
            return Err(Error::InvalidArchive(format!(
                "nonportable archive path {text}"
            )));
        }
    }
    Ok(())
}

fn validate_link_target(target: &Path, limits: &ArchiveLimits) -> Result<()> {
    let value = target
        .to_str()
        .ok_or_else(|| Error::InvalidArchive("link target is not UTF-8".into()))?;
    if value.is_empty()
        || value.len() > limits.max_path_bytes
        || target.components().count() > limits.max_depth
        || value.contains('\0')
        || value.contains('\\')
    {
        return Err(Error::InvalidArchive("invalid link target".into()));
    }
    Ok(())
}

fn invalid_windows_component(value: &str) -> bool {
    if value.ends_with(['.', ' ']) || value.contains(['<', '>', ':', '"', '|', '?', '*']) {
        return true;
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || stem.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
}

fn resolve_link(parent: &Path, target: &Path) -> Result<PathBuf> {
    if target.is_absolute()
        || target
            .components()
            .any(|part| matches!(part, Component::Prefix(_) | Component::RootDir))
    {
        return Err(Error::InvalidArchive("absolute link target".into()));
    }
    let mut output = parent
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => Some(value.to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => output.push(value.to_owned()),
            Component::ParentDir => {
                output
                    .pop()
                    .ok_or_else(|| Error::InvalidArchive("link target escapes archive".into()))?;
            }
            _ => return Err(Error::InvalidArchive("invalid link target".into())),
        }
    }
    Ok(output.into_iter().collect())
}

fn materialize_links(destination: &Path, manifest: &[ManifestEntry]) -> Result<()> {
    for item in manifest.iter().filter(|item| item.kind == Kind::Hardlink) {
        let output = destination.join(&item.path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        let target = resolve_link(
            Path::new(""),
            item.target.as_ref().expect("validated target"),
        )?;
        fs::hard_link(destination.join(target), &output).at(&output)?;
    }
    for item in manifest.iter().filter(|item| item.kind == Kind::Symlink) {
        let output = destination.join(&item.path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent).at(parent)?;
        }
        let target = item.target.as_ref().expect("validated target");
        let resolved = resolve_link(item.path.parent().unwrap_or(Path::new("")), target)?;
        let is_directory = manifest.iter().any(|candidate| {
            (candidate.path == resolved && candidate.kind == Kind::Directory)
                || (candidate.path.starts_with(&resolved) && candidate.path != resolved)
        });
        create_symlink(target, &output, is_directory)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, output: &Path, _: bool) -> Result<()> {
    std::os::unix::fs::symlink(target, output).at(output)
}

#[cfg(windows)]
fn create_symlink(target: &Path, output: &Path, is_directory: bool) -> Result<()> {
    if is_directory {
        std::os::windows::fs::symlink_dir(target, output).at(output)
    } else {
        std::os::windows::fs::symlink_file(target, output).at(output)
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777)).at(path)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: Option<u32>) -> Result<()> {
    Ok(())
}

struct ReadLimit<R> {
    inner: R,
    remaining: u64,
}

impl<R> ReadLimit<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<R: Read> Read for ReadLimit<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decompressed tar exceeds configured limit",
                )),
            };
        }
        let wanted = self.remaining.min(output.len() as u64) as usize;
        let count = self.inner.read(&mut output[..wanted])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_symlink_may_use_parent_without_escaping_package() {
        assert_eq!(
            resolve_link(Path::new("foo/libs"), Path::new("../lib/a.so")).unwrap(),
            PathBuf::from("foo/lib/a.so")
        );
    }

    #[test]
    fn rejects_windows_reserved_names_everywhere() {
        let limits = ArchiveLimits::default();
        assert!(validate_relative_path(Path::new("foo/NUL.txt"), &limits).is_err());
    }
}
