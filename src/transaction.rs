use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    CacheEntry, Error, InstallOutcome, InstalledPackage, InstallerOptions, LockPolicy,
    PackageMetadata, RemovalOutcome, Result,
    error::IoContext,
    fsutil::{
        copy_tree, hash_tree, owner_token, remove_with_retry, rename_with_retry, write_atomic,
    },
    metadata::{inspect_package_tree, validate_package_tree},
};

pub(crate) fn materialize(
    entry: &CacheEntry,
    library: &Path,
    options: &InstallerOptions,
) -> Result<InstallOutcome> {
    fs::create_dir_all(library).at(library)?;
    let global = acquire_global_if_strict(library, options)?;
    let mut transaction = Transaction::acquire(library, &entry.package.name, "install", options)?;

    if transaction
        .final_path
        .try_exists()
        .at(&transaction.final_path)?
        && inspect_package_tree(&transaction.final_path).ok().as_ref() == Some(&entry.package)
        && hash_tree(&transaction.final_path)? == entry.tree_digest
    {
        let package = InstalledPackage {
            root: transaction.final_path.clone(),
            metadata: entry.package.clone(),
        };
        transaction.finish_empty()?;
        drop(global);
        return Ok(InstallOutcome::AlreadyInstalled(package));
    }

    fs::create_dir_all(transaction.new_path.parent().expect("new path has parent"))
        .at(transaction.new_path.parent().expect("new path has parent"))?;
    copy_tree(&entry.package_root, &transaction.new_path)?;
    validate_package_tree(
        &transaction.new_path,
        &crate::ExpectedPackage {
            name: entry.package.name.clone(),
            version: entry.package.version.clone(),
            r_major_minor: None,
            platform: None,
            architecture: None,
        },
    )?;
    if hash_tree(&transaction.new_path)? != entry.tree_digest {
        return Err(Error::CacheCorrupt {
            path: entry.package_root.clone(),
            message: "materialized tree digest differs from cache manifest".into(),
        });
    }
    transaction.state("Prepared")?;

    let replaced = transaction
        .final_path
        .try_exists()
        .at(&transaction.final_path)?;
    if replaced {
        match rename_with_retry(
            &transaction.final_path,
            &transaction.old_path,
            transaction.retry,
        ) {
            Ok(()) => {
                transaction.preserve = true;
                transaction.state("OldMoved")?;
            }
            Err(source) => {
                return Err(Error::PackageInUse {
                    package: transaction.package.clone(),
                    source,
                });
            }
        }
    }

    if let Err(publish) = rename_with_retry(
        &transaction.new_path,
        &transaction.final_path,
        transaction.retry,
    ) {
        if replaced {
            match rename_with_retry(
                &transaction.old_path,
                &transaction.final_path,
                transaction.retry,
            ) {
                Ok(()) => {
                    transaction.preserve = false;
                    let _ = transaction.state("RolledBack");
                    return Err(Error::Io {
                        path: transaction.final_path.clone(),
                        source: publish,
                    });
                }
                Err(rollback) => {
                    return Err(Error::RecoveryRequired {
                        lock: transaction.lock_path.clone(),
                        publish: publish.to_string(),
                        rollback: rollback.to_string(),
                    });
                }
            }
        }
        return Err(Error::Io {
            path: transaction.final_path.clone(),
            source: publish,
        });
    }
    transaction.preserve = true;
    transaction.state("NewPublished")?;
    let published = validate_package_tree(
        &transaction.final_path,
        &crate::ExpectedPackage {
            name: entry.package.name.clone(),
            version: entry.package.version.clone(),
            r_major_minor: None,
            platform: None,
            architecture: None,
        },
    )
    .and_then(|metadata| {
        if hash_tree(&transaction.final_path)? == entry.tree_digest {
            Ok(metadata)
        } else {
            Err(Error::CacheCorrupt {
                path: transaction.final_path.clone(),
                message: "published tree failed digest validation".into(),
            })
        }
    });
    let metadata = match published {
        Ok(metadata) => metadata,
        Err(error) => {
            transaction.state("RollbackRequired")?;
            let remove_result = remove_with_retry(&transaction.final_path, transaction.retry);
            let rollback_result = if replaced && remove_result.is_ok() {
                rename_with_retry(
                    &transaction.old_path,
                    &transaction.final_path,
                    transaction.retry,
                )
            } else {
                remove_result
            };
            match rollback_result {
                Ok(()) => {
                    transaction.preserve = false;
                    let _ = transaction.state("RolledBack");
                    return Err(error);
                }
                Err(rollback) => {
                    return Err(Error::RecoveryRequired {
                        lock: transaction.lock_path.clone(),
                        publish: error.to_string(),
                        rollback: rollback.to_string(),
                    });
                }
            }
        }
    };
    transaction.state("Committed")?;
    let package = InstalledPackage {
        root: transaction.final_path.clone(),
        metadata,
    };
    if replaced && remove_with_retry(&transaction.old_path, transaction.retry).is_err() {
        transaction.preserve = true;
        return Ok(InstallOutcome::CommittedCleanupPending {
            package,
            lock: transaction.lock_path.clone(),
        });
    }
    transaction.preserve = false;
    transaction.finish_empty()?;
    drop(global);
    Ok(if replaced {
        InstallOutcome::Replaced(package)
    } else {
        InstallOutcome::Installed(package)
    })
}

pub(crate) fn remove(
    library: &Path,
    package: &str,
    options: &InstallerOptions,
) -> Result<RemovalOutcome> {
    validate_package_name(package)?;
    fs::create_dir_all(library).at(library)?;
    let global = acquire_global_if_strict(library, options)?;
    let mut transaction = Transaction::acquire(library, package, "remove", options)?;
    if !transaction
        .final_path
        .try_exists()
        .at(&transaction.final_path)?
    {
        transaction.finish_empty()?;
        drop(global);
        return Ok(RemovalOutcome::NotInstalled);
    }
    rename_with_retry(
        &transaction.final_path,
        &transaction.old_path,
        transaction.retry,
    )
    .map_err(|source| Error::PackageInUse {
        package: package.to_owned(),
        source,
    })?;
    transaction.preserve = true;
    transaction.state("RemovalMoved")?;
    transaction.state("RemovalCommitted")?;
    if remove_with_retry(&transaction.old_path, transaction.retry).is_err() {
        return Ok(RemovalOutcome::CommittedCleanupPending {
            lock: transaction.lock_path.clone(),
        });
    }
    transaction.preserve = false;
    transaction.finish_empty()?;
    drop(global);
    Ok(RemovalOutcome::Removed)
}

struct Transaction {
    lock_path: PathBuf,
    final_path: PathBuf,
    old_path: PathBuf,
    new_path: PathBuf,
    package: String,
    token: String,
    retry: Duration,
    preserve: bool,
    state_sequence: u8,
}

impl Transaction {
    fn acquire(
        library: &Path,
        package: &str,
        operation: &str,
        options: &InstallerOptions,
    ) -> Result<Self> {
        validate_package_name(package)?;
        let global = library.join("00LOCK");
        if options.lock_policy == LockPolicy::Package && global.try_exists().at(&global)? {
            return Err(Error::LibraryLocked(global));
        }
        let lock_path = library.join(format!("00LOCK-{package}"));
        match fs::create_dir(&lock_path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::PackageLocked {
                    package: package.to_owned(),
                    path: lock_path,
                });
            }
            Err(source) => {
                return Err(Error::Io {
                    path: lock_path,
                    source,
                });
            }
        }
        if options.lock_policy == LockPolicy::Package && global.try_exists().at(&global)? {
            let _ = fs::remove_dir(&lock_path);
            return Err(Error::LibraryLocked(global));
        }
        let token = owner_token();
        if let Err(error) = write_atomic(
            &lock_path.join("owner.dcf"),
            format!(
                "Format: 1\nPackage: {package}\nPid: {}\nToken: {token}\nOperation: {operation}\n",
                std::process::id()
            )
            .as_bytes(),
        ) {
            let _ = fs::remove_dir(&lock_path);
            return Err(error);
        }
        let mut transaction = Self {
            final_path: library.join(package),
            old_path: lock_path.join(package),
            new_path: lock_path.join("00new").join(package),
            lock_path,
            package: package.to_owned(),
            token,
            retry: options.windows_retry_timeout,
            preserve: false,
            state_sequence: 0,
        };
        transaction.state("Locked")?;
        Ok(transaction)
    }

    fn state(&mut self, state: &str) -> Result<()> {
        self.state_sequence = self.state_sequence.saturating_add(1);
        write_atomic(
            &self
                .lock_path
                .join(format!("state-{:03}.dcf", self.state_sequence)),
            format!(
                "Format: 1\nPackage: {}\nToken: {}\nState: {state}\n",
                self.package, self.token
            )
            .as_bytes(),
        )
    }

    fn finish_empty(&mut self) -> Result<()> {
        self.preserve = false;
        remove_with_retry(&self.lock_path, self.retry).at(&self.lock_path)
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        if !self.preserve && owns_lock(&self.lock_path, &self.token) {
            let _ = remove_with_retry(&self.lock_path, self.retry);
        }
    }
}

struct GlobalGuard {
    path: PathBuf,
    token: String,
    retry: Duration,
}

impl Drop for GlobalGuard {
    fn drop(&mut self) {
        if owns_lock(&self.path, &self.token) {
            let _ = remove_with_retry(&self.path, self.retry);
        }
    }
}

fn acquire_global_if_strict(
    library: &Path,
    options: &InstallerOptions,
) -> Result<Option<GlobalGuard>> {
    if options.lock_policy != LockPolicy::StrictLibrary {
        return Ok(None);
    }
    let path = library.join("00LOCK");
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::LibraryLocked(path));
        }
        Err(source) => return Err(Error::Io { path, source }),
    }
    let token = owner_token();
    if let Err(error) = write_atomic(
        &path.join("owner.dcf"),
        format!(
            "Format: 1\nPid: {}\nToken: {token}\nOperation: strict-library-lock\n",
            std::process::id()
        )
        .as_bytes(),
    ) {
        let _ = fs::remove_dir(&path);
        return Err(error);
    }
    Ok(Some(GlobalGuard {
        path,
        token,
        retry: options.windows_retry_timeout,
    }))
}

fn owns_lock(path: &Path, token: &str) -> bool {
    fs::read_to_string(path.join("owner.dcf"))
        .ok()
        .is_some_and(|source| source.lines().any(|line| line == format!("Token: {token}")))
}

fn validate_package_name(package: &str) -> Result<()> {
    if package.len() < 2
        || !package.as_bytes()[0].is_ascii_alphabetic()
        || !package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
        || package.ends_with('.')
    {
        return Err(Error::InvalidMetadata {
            path: PathBuf::from(package),
            message: "illegal R package name".into(),
        });
    }
    Ok(())
}

pub(crate) fn installed(path: PathBuf, metadata: PackageMetadata) -> InstalledPackage {
    InstalledPackage {
        root: path,
        metadata,
    }
}
