use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

use r_dcf_syntax::parse;

use crate::{
    Artifact, CacheEntry, Digest, Error, InstallerOptions, PackageMetadata, PrepareRequest, Result,
    archive::extract_binary,
    error::IoContext,
    fsutil::{
        copy_and_hash_file, copy_tree, hash_tree, owner_token, remove_with_retry,
        rename_with_retry, validate_tree_structure, write_atomic,
    },
    metadata::validate_package_tree,
    platform::validate_native_code,
    source::materialize_source,
};

const MANIFEST: &str = "manifest.dcf";
const COMPLETE: &str = "COMPLETE";

/// A caller-owned immutable package cache.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Creates a cache rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the cache root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Looks up and validates the entry described by `request`.
    ///
    /// Returns `None` when no cache object exists for the request key.
    pub fn lookup(&self, request: &PrepareRequest) -> Result<Option<CacheEntry>> {
        let root = self.objects().join(request.key.to_string());
        if !root.try_exists().at(&root)? {
            return Ok(None);
        }
        read_entry(&root, request).map(Some)
    }

    pub(crate) fn prepare(
        &self,
        request: &PrepareRequest,
        options: &InstallerOptions,
    ) -> Result<CacheEntry> {
        self.initialize()?;
        if let Some(entry) = self.lookup(request)? {
            return Ok(entry);
        }

        let lock_path = self.locks().join(request.key.to_string());
        let started = Instant::now();
        let token = loop {
            match fs::create_dir(&lock_path) {
                Ok(()) => {
                    let token = owner_token();
                    if let Err(error) = write_atomic(
                        &lock_path.join("owner.dcf"),
                        format!(
                            "Format: 1\nKey: {}\nPid: {}\nToken: {}\nOperation: prepare\n",
                            request.key,
                            std::process::id(),
                            token
                        )
                        .as_bytes(),
                    ) {
                        let _ = fs::remove_dir(&lock_path);
                        return Err(error);
                    }
                    break token;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if let Some(entry) = self.lookup(request)? {
                        return Ok(entry);
                    }
                    if started.elapsed() >= options.cache_lock.wait_timeout {
                        return Err(Error::CacheLocked {
                            key: request.key.to_string(),
                            path: lock_path,
                        });
                    }
                    thread::sleep(options.cache_lock.poll_interval);
                }
                Err(source) => {
                    return Err(Error::Io {
                        path: lock_path,
                        source,
                    });
                }
            }
        };
        let guard = CacheLockGuard {
            path: lock_path,
            token,
            retry: options.windows_retry_timeout,
        };
        if let Some(entry) = self.lookup(request)? {
            return Ok(entry);
        }

        let build_root = self
            .builds()
            .join(format!("{}-{}", request.key, owner_token()));
        fs::create_dir(&build_root).at(&build_root)?;
        let build_guard = BuildGuard {
            path: build_root.clone(),
            retry: options.windows_retry_timeout,
        };
        let input_directory = build_root.join("input");
        fs::create_dir(&input_directory).at(&input_directory)?;
        let source_path = artifact_path(&request.artifact);
        let filename = source_path
            .file_name()
            .ok_or_else(|| Error::Unsupported("artifact path has no filename".into()))?;
        let private_artifact_path = input_directory.join(filename);
        let actual = copy_and_hash_file(source_path, &private_artifact_path)?;
        if actual != request.artifact_digest {
            return Err(Error::DigestMismatch {
                expected: request.artifact_digest.to_string(),
                actual: actual.to_string(),
            });
        }
        let private_artifact = private_artifact(&request.artifact, private_artifact_path);
        let object = build_root.join("object");
        fs::create_dir(&object).at(&object)?;
        let prepared = match &private_artifact {
            Artifact::Binary(artifact) => extract_binary(
                artifact,
                &object,
                &request.expected,
                &options.archive_limits,
            )?,
            Artifact::Source(artifact) => {
                let package = materialize_source(artifact, &build_root, &request.expected)?;
                let destination = object.join(&request.expected.name);
                copy_tree(&package, &destination)?;
                destination
            }
        };
        validate_tree_structure(&prepared)?;
        let package = validate_package_tree(&prepared, &request.expected)?;
        validate_native_code(&prepared, &request.expected)?;
        let tree_digest = hash_tree(&prepared)?;
        write_manifest(&object, request, &package, tree_digest)?;
        write_atomic(&object.join(COMPLETE), b"complete\n")?;

        let final_root = self.objects().join(request.key.to_string());
        match rename_with_retry(&object, &final_root, options.windows_retry_timeout) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_entry(&final_root, request)?;
                if existing.tree_digest != tree_digest {
                    return Err(Error::CacheCorrupt {
                        path: final_root,
                        message: "cache race produced a different tree".into(),
                    });
                }
            }
            Err(source) => {
                return Err(Error::Io {
                    path: final_root,
                    source,
                });
            }
        }
        drop(build_guard);
        drop(guard);
        read_entry(&final_root, request)
    }

    fn initialize(&self) -> Result<()> {
        for path in [&self.root, &self.objects(), &self.builds(), &self.locks()] {
            fs::create_dir_all(path).at(path)?;
        }
        Ok(())
    }

    fn objects(&self) -> PathBuf {
        self.root.join("objects")
    }

    fn builds(&self) -> PathBuf {
        self.root.join(".build")
    }

    fn locks(&self) -> PathBuf {
        self.root.join("locks")
    }
}

fn artifact_path(artifact: &Artifact) -> &Path {
    match artifact {
        Artifact::Binary(value) => &value.path,
        Artifact::Source(value) => &value.path,
    }
}

fn private_artifact(artifact: &Artifact, path: PathBuf) -> Artifact {
    match artifact {
        Artifact::Binary(value) => Artifact::Binary(crate::BinaryArtifact {
            path,
            format: value.format,
        }),
        Artifact::Source(value) => {
            let mut value = value.clone();
            value.path = path;
            Artifact::Source(value)
        }
    }
}

fn write_manifest(
    root: &Path,
    request: &PrepareRequest,
    package: &PackageMetadata,
    tree_digest: Digest,
) -> Result<()> {
    let kind = match request.artifact {
        Artifact::Binary(_) => "binary",
        Artifact::Source(_) => "source",
    };
    let contents = format!(
        "Format: 1\nKey: {}\nPackage: {}\nVersion: {}\nKind: {}\nArtifact-SHA256: {}\nTree-SHA256: {}\n",
        request.key, package.name, package.version, kind, request.artifact_digest, tree_digest
    );
    write_atomic(&root.join(MANIFEST), contents.as_bytes())
}

fn read_entry(root: &Path, request: &PrepareRequest) -> Result<CacheEntry> {
    if !root.join(COMPLETE).is_file() {
        return Err(Error::CacheCorrupt {
            path: root.to_owned(),
            message: "missing COMPLETE marker".into(),
        });
    }
    let manifest_path = root.join(MANIFEST);
    let source = fs::read_to_string(&manifest_path).at(&manifest_path)?;
    let parsed = parse(&source);
    if let Some(diagnostic) = parsed.diagnostics().first() {
        return Err(Error::CacheCorrupt {
            path: manifest_path,
            message: diagnostic.message().to_owned(),
        });
    }
    let record = parsed.records().next().ok_or_else(|| Error::CacheCorrupt {
        path: manifest_path.clone(),
        message: "manifest has no record".into(),
    })?;
    let field = |name: &str| {
        record
            .last_field(name)
            .map(|field| field.value().as_str().trim().to_owned())
    };
    for (name, expected) in [
        ("Key", request.key.to_string()),
        ("Package", request.expected.name.clone()),
        ("Version", request.expected.version.clone()),
        ("Artifact-SHA256", request.artifact_digest.to_string()),
    ] {
        if field(name).as_deref() != Some(expected.as_str()) {
            return Err(Error::CacheCorrupt {
                path: manifest_path,
                message: format!("manifest {name} does not match request"),
            });
        }
    }
    let tree_digest =
        Digest::from_hex(&field("Tree-SHA256").ok_or_else(|| Error::CacheCorrupt {
            path: manifest_path.clone(),
            message: "manifest is missing Tree-SHA256".into(),
        })?)?;
    let package_root = root.join(&request.expected.name);
    let package = validate_package_tree(&package_root, &request.expected)?;
    let actual_tree_digest = hash_tree(&package_root)?;
    if actual_tree_digest != tree_digest {
        return Err(Error::CacheCorrupt {
            path: package_root,
            message: format!(
                "tree digest mismatch: manifest has {tree_digest}, found {actual_tree_digest}"
            ),
        });
    }
    Ok(CacheEntry {
        key: request.key,
        root: root.to_owned(),
        package_root,
        package,
        tree_digest,
    })
}

struct BuildGuard {
    path: PathBuf,
    retry: std::time::Duration,
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        let _ = remove_with_retry(&self.path, self.retry);
    }
}

struct CacheLockGuard {
    path: PathBuf,
    token: String,
    retry: std::time::Duration,
}

impl Drop for CacheLockGuard {
    fn drop(&mut self) {
        let owner = self.path.join("owner.dcf");
        let owned = fs::read_to_string(owner).ok().is_some_and(|source| {
            source
                .lines()
                .any(|line| line == format!("Token: {}", self.token))
        });
        if owned {
            let _ = remove_with_retry(&self.path, self.retry);
        }
    }
}
