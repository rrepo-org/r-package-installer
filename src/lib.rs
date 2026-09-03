//! Transactional installation of predownloaded R package artifacts.
//!
//! This crate deliberately does not download or resolve packages. Each call
//! prepares or publishes one package and is safe to schedule concurrently with
//! calls for other packages.
#![deny(unsafe_code, missing_docs)]

mod archive;
mod cache;
mod error;
mod fsutil;
mod library;
mod metadata;
mod model;
mod platform;
mod source;
mod transaction;

pub use cache::Cache;
pub use error::{Error, Result};
pub use library::{recover_package, remove_package, scan_library};
pub use model::{
    ArchiveLimits, Artifact, BinaryArtifact, BinaryFormat, CacheEntry, CacheKey, CacheLockPolicy,
    CancellationToken, Digest, ExpectedPackage, InstallOutcome, InstalledPackage, InstallerOptions,
    LibraryEntry, LockMetadata, LockPolicy, PackageMetadata, PrepareRequest, RecoveryAction,
    RecoveryPlan, RemovalOutcome, SourceArtifact, SourceOptions,
};

use std::path::Path;

/// A reusable handle for a caller-owned package cache.
#[derive(Debug, Clone)]
pub struct Installer {
    cache: Cache,
    options: InstallerOptions,
}

impl Installer {
    /// Creates an installer rooted at `cache_root`.
    pub fn new(cache_root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            cache: Cache::new(cache_root),
            options: InstallerOptions::default(),
        }
    }

    /// Creates an installer with explicit operational policy.
    pub fn with_options(
        cache_root: impl Into<std::path::PathBuf>,
        options: InstallerOptions,
    ) -> Self {
        Self {
            cache: Cache::new(cache_root),
            options,
        }
    }

    /// Returns the caller-owned cache configuration.
    pub const fn cache(&self) -> &Cache {
        &self.cache
    }

    /// Returns a validated immutable cache entry, building it on a miss.
    pub fn prepare(&self, request: &PrepareRequest) -> Result<CacheEntry> {
        self.cache.prepare(request, &self.options)
    }

    /// Publishes one cached package into an R library transactionally.
    pub fn materialize(&self, entry: &CacheEntry, library: &Path) -> Result<InstallOutcome> {
        transaction::materialize(entry, library, &self.options)
    }

    /// Prepares and publishes one package.
    pub fn install(&self, request: &PrepareRequest, library: &Path) -> Result<InstallOutcome> {
        let entry = self.prepare(request)?;
        self.materialize(&entry, library)
    }

    /// Removes one package using the same backup transaction as installation.
    pub fn remove(&self, library: &Path, package: &str) -> Result<RemovalOutcome> {
        remove_package(library, package, &self.options)
    }
}
