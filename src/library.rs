use std::{fs, path::Path};

use r_dcf_syntax::parse;

use crate::{
    Error, InstallerOptions, LibraryEntry, LockMetadata, RecoveryAction, RecoveryPlan,
    RemovalOutcome, Result,
    error::IoContext,
    fsutil::{remove_with_retry, rename_with_retry},
    metadata::inspect_package_tree,
    transaction,
};

/// Inspects installed packages, locks, and incomplete entries in an R library.
pub fn scan_library(library: &Path) -> Result<Vec<LibraryEntry>> {
    if !library.try_exists().at(library)? {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(library)
        .at(library)?
        .map(|entry| entry.at(library).map(|entry| entry.path()))
        .collect::<Result<Vec<_>>>()?;
    paths.sort();
    let mut output = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path.file_name().and_then(|value| value.to_str());
        if name == Some("00LOCK") || name.is_some_and(|value| value.starts_with("00LOCK-")) {
            let lock = read_lock(&path)?;
            let plan = recovery_plan(lock.clone(), library);
            if plan.action == RecoveryAction::Manual {
                output.push(LibraryEntry::Locked(lock));
            } else {
                output.push(LibraryEntry::Recoverable(plan));
            }
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) => {
                output.push(LibraryEntry::Incomplete {
                    path,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if !metadata.is_dir() && !metadata.file_type().is_symlink() {
            output.push(LibraryEntry::Foreign(path));
            continue;
        }
        match inspect_package_tree(&path) {
            Ok(metadata) => output.push(LibraryEntry::Installed(transaction::installed(
                path, metadata,
            ))),
            Err(error) => output.push(LibraryEntry::Incomplete {
                path,
                reason: error.to_string(),
            }),
        }
    }
    Ok(output)
}

/// Transactionally removes `package` from an R library.
pub fn remove_package(
    library: &Path,
    package: &str,
    options: &InstallerOptions,
) -> Result<RemovalOutcome> {
    transaction::remove(library, package, options)
}

/// Applies a recovery plan after the caller has established that its owner is
/// no longer running. The token and filesystem-derived action are rechecked,
/// but PID reuse makes abandonment impossible to prove portably.
pub fn recover_package(plan: &RecoveryPlan, options: &InstallerOptions) -> Result<()> {
    let current = read_lock(&plan.lock.path)?;
    if current.token.is_none()
        || current.token != plan.lock.token
        || current.package != plan.lock.package
    {
        return Err(Error::InvalidRecovery {
            path: plan.lock.path.clone(),
            message: "lock ownership changed or is foreign".into(),
        });
    }
    let current_plan = recovery_plan(
        current.clone(),
        current
            .path
            .parent()
            .ok_or_else(|| Error::InvalidRecovery {
                path: current.path.clone(),
                message: "lock has no library parent".into(),
            })?,
    );
    if current_plan.action != plan.action {
        return Err(Error::InvalidRecovery {
            path: current.path,
            message: "filesystem state changed since the recovery plan was created".into(),
        });
    }
    let claim_path = plan.lock.path.join("recovery.claim");
    let claim = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&claim_path)
        .at(&claim_path)?;
    drop(claim);
    let _claim_guard = ClaimGuard(claim_path);
    let package = current
        .package
        .as_deref()
        .ok_or_else(|| Error::InvalidRecovery {
            path: current.path.clone(),
            message: "lock does not identify a package".into(),
        })?;
    let library = current
        .path
        .parent()
        .ok_or_else(|| Error::InvalidRecovery {
            path: current.path.clone(),
            message: "lock has no library parent".into(),
        })?;
    let final_path = library.join(package);
    let old_path = current.path.join(package);
    let new_path = current.path.join("00new").join(package);
    match plan.action {
        RecoveryAction::RestoreBackup => {
            if final_path.try_exists().at(&final_path)? || !old_path.try_exists().at(&old_path)? {
                return Err(Error::InvalidRecovery {
                    path: current.path,
                    message: "backup cannot be restored in the current filesystem state".into(),
                });
            }
            rename_with_retry(&old_path, &final_path, options.windows_retry_timeout)
                .at(&final_path)?;
            remove_with_retry(&plan.lock.path, options.windows_retry_timeout).at(&plan.lock.path)
        }
        RecoveryAction::RollbackPublication => {
            remove_with_retry(&final_path, options.windows_retry_timeout).at(&final_path)?;
            if old_path.try_exists().at(&old_path)? {
                rename_with_retry(&old_path, &final_path, options.windows_retry_timeout)
                    .at(&final_path)?;
            }
            remove_with_retry(&plan.lock.path, options.windows_retry_timeout).at(&plan.lock.path)
        }
        RecoveryAction::FinishCommit => {
            inspect_package_tree(&final_path)?;
            remove_with_retry(&old_path, options.windows_retry_timeout).at(&old_path)?;
            remove_with_retry(&plan.lock.path, options.windows_retry_timeout).at(&plan.lock.path)
        }
        RecoveryAction::RemoveStaging => {
            if old_path.try_exists().at(&old_path)? {
                return Err(Error::InvalidRecovery {
                    path: current.path,
                    message: "refusing to remove staging while a backup exists".into(),
                });
            }
            let _ = remove_with_retry(&new_path, options.windows_retry_timeout);
            remove_with_retry(&plan.lock.path, options.windows_retry_timeout).at(&plan.lock.path)
        }
        RecoveryAction::FinishRemoval => {
            if final_path.try_exists().at(&final_path)? {
                return Err(Error::InvalidRecovery {
                    path: current.path,
                    message: "cannot finish removal while final package exists".into(),
                });
            }
            remove_with_retry(&old_path, options.windows_retry_timeout).at(&old_path)?;
            remove_with_retry(&plan.lock.path, options.windows_retry_timeout).at(&plan.lock.path)
        }
        RecoveryAction::Manual => Err(Error::InvalidRecovery {
            path: current.path,
            message: "ambiguous or foreign lock requires manual recovery".into(),
        }),
    }
}

fn read_lock(path: &Path) -> Result<LockMetadata> {
    let owner_path = path.join("owner.dcf");
    let owner = fs::read_to_string(&owner_path).ok();
    let mut state_paths = fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|entry| {
            entry
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("state-") && name.ends_with(".dcf"))
        })
        .collect::<Vec<_>>();
    state_paths.sort();
    let state = state_paths
        .last()
        .and_then(|path| fs::read_to_string(path).ok());
    let field = |source: Option<&String>, name: &str| {
        source.and_then(|source| {
            let parsed = parse(source);
            parsed
                .records()
                .next()?
                .last_field(name)
                .map(|field| field.value().as_str().trim().to_owned())
        })
    };
    Ok(LockMetadata {
        path: path.to_owned(),
        package: field(owner.as_ref(), "Package").or_else(|| field(state.as_ref(), "Package")),
        token: field(owner.as_ref(), "Token"),
        operation: field(owner.as_ref(), "Operation"),
        state: field(state.as_ref(), "State"),
    })
}

fn recovery_plan(lock: LockMetadata, library: &Path) -> RecoveryPlan {
    let action = lock
        .package
        .as_ref()
        .map_or(RecoveryAction::Manual, |package| {
            let final_exists = library.join(package).exists();
            let old_exists = lock.path.join(package).exists();
            let new_exists = lock.path.join("00new").join(package).exists();
            match lock.state.as_deref() {
                Some("OldMoved") if !final_exists && old_exists => RecoveryAction::RestoreBackup,
                Some("RollbackRequired") if final_exists => RecoveryAction::RollbackPublication,
                Some("NewPublished" | "Committed") if final_exists && old_exists => {
                    RecoveryAction::FinishCommit
                }
                Some("RemovalMoved") if !final_exists && old_exists => {
                    RecoveryAction::RestoreBackup
                }
                Some("RemovalCommitted") if !final_exists && old_exists => {
                    RecoveryAction::FinishRemoval
                }
                Some("Locked" | "Prepared" | "RolledBack")
                    if !old_exists && (new_exists || !final_exists) =>
                {
                    RecoveryAction::RemoveStaging
                }
                _ => RecoveryAction::Manual,
            }
        });
    RecoveryPlan { lock, action }
}

struct ClaimGuard(std::path::PathBuf);

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
