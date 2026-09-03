use std::path::PathBuf;

/// Result type used by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Structured installer failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed at a known path.
    #[error("I/O failure at {path}: {source}")]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// A digest or cache key was not 64 hexadecimal characters.
    #[error("invalid cache key: {0}")]
    InvalidCacheKey(String),
    /// The artifact contents did not match the caller-supplied digest.
    #[error("artifact digest mismatch: expected {expected}, found {actual}")]
    DigestMismatch {
        /// Expected SHA-256 digest.
        expected: String,
        /// Actual SHA-256 digest.
        actual: String,
    },
    /// An archive failed structural or resource-limit validation.
    #[error("invalid archive: {0}")]
    InvalidArchive(String),
    /// A package metadata file or installed-tree marker was invalid.
    #[error("invalid package metadata in {path}: {message}")]
    InvalidMetadata {
        /// Path containing the invalid metadata.
        path: PathBuf,
        /// Description of the validation failure.
        message: String,
    },
    /// The package name differed from the authoritative request.
    #[error("package identity mismatch: expected {expected}, found {actual}")]
    PackageMismatch {
        /// Expected package name.
        expected: String,
        /// Package name found in the artifact.
        actual: String,
    },
    /// The package version differed from the authoritative request.
    #[error("package version mismatch: expected {expected}, found {actual}")]
    VersionMismatch {
        /// Expected package version.
        expected: String,
        /// Package version found in the artifact.
        actual: String,
    },
    /// A package does not satisfy the requested target constraints.
    #[error("package is incompatible with this target: {0}")]
    Incompatible(String),
    /// Another process held the requested cache-key lock until timeout.
    #[error("cache key {key} is locked at {path}")]
    CacheLocked {
        /// Contended cache key.
        key: String,
        /// Path to the cache lock.
        path: PathBuf,
    },
    /// The target library has a global `00LOCK` directory.
    #[error("R library is locked at {0}")]
    LibraryLocked(PathBuf),
    /// The target package has a `00LOCK-<package>` directory.
    #[error("package {package} is locked at {path}")]
    PackageLocked {
        /// Locked package name.
        package: String,
        /// Path to the package lock.
        path: PathBuf,
    },
    /// An existing cache object failed validation.
    #[error("cache entry is corrupt at {path}: {message}")]
    CacheCorrupt {
        /// Root or metadata path of the corrupt object.
        path: PathBuf,
        /// Description of the failed invariant.
        message: String,
    },
    /// `R CMD INSTALL` exited unsuccessfully.
    #[error("R CMD INSTALL failed with status {status:?}\n{output}")]
    RInstallFailed {
        /// Child exit status, or `None` when unavailable.
        status: Option<i32>,
        /// Bounded captured standard output and error.
        output: String,
    },
    /// A source installation exceeded its configured timeout.
    #[error("R CMD INSTALL timed out")]
    RInstallTimeout,
    /// A source installation observed its cancellation token.
    #[error("R CMD INSTALL was cancelled")]
    RInstallCancelled,
    /// A package tree could not be moved after bounded retries.
    #[error("package {package} appears to be in use: {source}")]
    PackageInUse {
        /// Package that could not be moved.
        package: String,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Atomic publication would require crossing filesystem boundaries.
    #[error("cross-device publication is not supported: {0}")]
    CrossDevice(PathBuf),
    /// Publication and its rollback both failed, leaving recovery evidence.
    #[error("publication failed and rollback also failed; recovery required at {lock}")]
    RecoveryRequired {
        /// Preserved transaction lock.
        lock: PathBuf,
        /// Original publication failure.
        publish: String,
        /// Rollback failure.
        rollback: String,
    },
    /// A recovery plan was foreign, stale, or inconsistent.
    #[error("invalid recovery state at {path}: {message}")]
    InvalidRecovery {
        /// Lock or package path involved in recovery.
        path: PathBuf,
        /// Description of the failed recovery invariant.
        message: String,
    },
    /// The requested operation or artifact characteristic is unsupported.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        let path = path.into();
        self.map_err(|source| Error::Io { path, source })
    }
}
