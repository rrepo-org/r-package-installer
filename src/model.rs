use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{Error, Result};

/// A SHA-256 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// Creates a digest from its 32-byte representation.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses a digest from 64 hexadecimal characters.
    pub fn from_hex(value: &str) -> Result<Self> {
        let bytes = hex::decode(value).map_err(|_| Error::InvalidCacheKey(value.to_owned()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| Error::InvalidCacheKey(value.to_owned()))?;
        Ok(Self(bytes))
    }

    /// Returns the digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

/// A caller-defined source or binary cache identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(Digest);

impl CacheKey {
    /// Creates a cache key from a digest computed by the caller.
    pub const fn from_digest(digest: Digest) -> Self {
        Self(digest)
    }

    /// Parses a cache key from 64 hexadecimal characters.
    pub fn from_hex(value: &str) -> Result<Self> {
        Digest::from_hex(value).map(Self)
    }
}

impl fmt::Display for CacheKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Authoritative identity and compatibility information supplied upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedPackage {
    /// Expected `Package` field and top-level directory name.
    pub name: String,
    /// Expected `Version` field.
    pub version: String,
    /// Optional required R major and minor version, such as `4.5`.
    pub r_major_minor: Option<String>,
    /// Optional required platform from the `Built` field.
    pub platform: Option<String>,
    /// Optional architecture that must occur in the package platform.
    pub architecture: Option<String>,
}

/// A supported prebuilt archive container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormat {
    /// A ZIP package archive, normally used by Windows binary packages.
    Zip,
    /// A gzip-compressed tar package archive.
    TarGz,
}

/// A prebuilt R package archive.
#[derive(Debug, Clone)]
pub struct BinaryArtifact {
    /// Path to the predownloaded archive.
    pub path: PathBuf,
    /// Archive container format.
    pub format: BinaryFormat,
}

/// Per-child cancellation state. Cancelling does not affect other calls.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Requests cancellation of the associated source installation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Controls an `R CMD INSTALL` source build.
#[derive(Debug, Clone)]
pub struct SourceOptions {
    /// R executable used to launch `R CMD INSTALL`.
    pub r_executable: PathBuf,
    /// Library paths exposed to R as source-build dependencies.
    pub dependency_libraries: Vec<PathBuf>,
    /// Additional environment variables for the source-build process.
    pub environment: BTreeMap<String, String>,
    /// Arguments joined and passed through `--configure-args`.
    pub configure_args: Vec<String>,
    /// Maximum duration allowed for the source-build process.
    pub timeout: Duration,
    /// Cancellation state observed while the source build is running.
    pub cancellation: CancellationToken,
    /// Allows packages declaring `StagedInstall: no` when set.
    pub allow_non_staged: bool,
}

impl Default for SourceOptions {
    fn default() -> Self {
        Self {
            r_executable: PathBuf::from("R"),
            dependency_libraries: Vec::new(),
            environment: BTreeMap::new(),
            configure_args: Vec::new(),
            timeout: Duration::from_secs(30 * 60),
            cancellation: CancellationToken::default(),
            allow_non_staged: false,
        }
    }
}

/// A predownloaded R source package archive and its build options.
#[derive(Debug, Clone)]
pub struct SourceArtifact {
    /// Path to the source archive.
    pub path: PathBuf,
    /// Source installation policy.
    pub options: SourceOptions,
}

/// A package artifact to prepare.
#[derive(Debug, Clone)]
pub enum Artifact {
    /// A prebuilt package archive extracted natively by this crate.
    Binary(BinaryArtifact),
    /// A source package installed by `R CMD INSTALL`.
    Source(SourceArtifact),
}

/// Inputs required to prepare one immutable cache entry.
#[derive(Debug, Clone)]
pub struct PrepareRequest {
    /// Caller-defined identity for all inputs affecting the prepared tree.
    pub key: CacheKey,
    /// Expected SHA-256 digest of the artifact file.
    pub artifact_digest: Digest,
    /// Authoritative package identity and compatibility constraints.
    pub expected: ExpectedPackage,
    /// Predownloaded artifact to prepare.
    pub artifact: Artifact,
}

/// Locking policy used while modifying a target R library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPolicy {
    /// Acquire only base-R-compatible `00LOCK-<package>` locks.
    Package,
    /// Also acquire global `00LOCK` to serialize all library modifications.
    StrictLibrary,
}

/// Waiting policy for another process preparing the same cache key.
#[derive(Debug, Clone)]
pub struct CacheLockPolicy {
    /// Maximum time to wait for the cache entry to become available.
    pub wait_timeout: Duration,
    /// Delay between cache lookup attempts while waiting.
    pub poll_interval: Duration,
}

impl Default for CacheLockPolicy {
    fn default() -> Self {
        Self {
            wait_timeout: Duration::from_secs(5 * 60),
            poll_interval: Duration::from_millis(100),
        }
    }
}

/// Operational policy shared by installer calls.
#[derive(Debug, Clone)]
pub struct InstallerOptions {
    /// Locking policy for target libraries.
    pub lock_policy: LockPolicy,
    /// Waiting policy for cache-key contention.
    pub cache_lock: CacheLockPolicy,
    /// Maximum time spent retrying transient Windows filesystem failures.
    pub windows_retry_timeout: Duration,
    /// Resource and path limits enforced during archive extraction.
    pub archive_limits: ArchiveLimits,
}

impl Default for InstallerOptions {
    fn default() -> Self {
        Self {
            lock_policy: LockPolicy::Package,
            cache_lock: CacheLockPolicy::default(),
            windows_retry_timeout: Duration::from_secs(5),
            archive_limits: ArchiveLimits::default(),
        }
    }
}

/// Resource and path limits for untrusted package archives.
#[derive(Debug, Clone)]
pub struct ArchiveLimits {
    /// Maximum number of archive entries.
    pub max_entries: usize,
    /// Maximum total uncompressed size in bytes.
    pub max_expanded_size: u64,
    /// Maximum size of one regular file in bytes.
    pub max_file_size: u64,
    /// Maximum UTF-8 path length in bytes.
    pub max_path_bytes: usize,
    /// Maximum number of path components.
    pub max_depth: usize,
    /// Maximum ratio of expanded bytes to archive bytes.
    pub max_expansion_ratio: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_expanded_size: 8 * 1024 * 1024 * 1024,
            max_file_size: 2 * 1024 * 1024 * 1024,
            max_path_bytes: 4096,
            max_depth: 64,
            max_expansion_ratio: 1_000,
        }
    }
}

/// Metadata read from a validated installed package tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMetadata {
    /// Package name.
    pub name: String,
    /// Package version.
    pub version: String,
    /// Complete `Built` field.
    pub built: String,
    /// Optional `OS_type` field.
    pub os_type: Option<String>,
    /// Optional `Archs` field.
    pub archs: Option<String>,
}

/// A validated immutable cache object.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Cache key that identifies the object.
    pub key: CacheKey,
    /// Root containing the manifest, completion marker, and package tree.
    pub root: PathBuf,
    /// Root of the cached installed package tree.
    pub package_root: PathBuf,
    /// Validated package metadata.
    pub package: PackageMetadata,
    /// Deterministic digest of the package tree.
    pub tree_digest: Digest,
}

/// A package present in a target R library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    /// Root of the installed package tree.
    pub root: PathBuf,
    /// Validated package metadata.
    pub metadata: PackageMetadata,
}

/// Result of publishing a package into an R library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The package was newly installed.
    Installed(InstalledPackage),
    /// An existing package was replaced.
    Replaced(InstalledPackage),
    /// The exact requested package tree was already installed.
    AlreadyInstalled(InstalledPackage),
    /// Publication committed, but removal of the backup could not finish.
    CommittedCleanupPending {
        /// Successfully published package.
        package: InstalledPackage,
        /// Preserved transaction lock containing recovery evidence.
        lock: PathBuf,
    },
}

/// Result of transactionally removing a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalOutcome {
    /// No package existed at the requested path.
    NotInstalled,
    /// The package was removed and transaction cleanup completed.
    Removed,
    /// Removal committed, but backup cleanup could not finish.
    CommittedCleanupPending {
        /// Preserved transaction lock containing recovery evidence.
        lock: PathBuf,
    },
}

/// Metadata read from an R library lock directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockMetadata {
    /// Path to the lock directory.
    pub path: PathBuf,
    /// Package recorded by an installer-owned lock, if available.
    pub package: Option<String>,
    /// Ownership token recorded by an installer-owned lock, if available.
    pub token: Option<String>,
    /// Operation recorded by an installer-owned lock, if available.
    pub operation: Option<String>,
    /// Latest durable transaction state, if available.
    pub state: Option<String>,
}

/// Filesystem action selected for an interrupted transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Restore the previous package after publication did not begin.
    RestoreBackup,
    /// Remove a failed publication and restore any previous package.
    RollbackPublication,
    /// Keep the published package and remove its backup.
    FinishCommit,
    /// Remove an unpublished staged package.
    RemoveStaging,
    /// Finish deleting a package whose removal already committed.
    FinishRemoval,
    /// Require caller-directed recovery because the state is ambiguous.
    Manual,
}

/// A proposed recovery action derived from lock metadata and filesystem state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    /// Lock metadata observed during the library scan.
    pub lock: LockMetadata,
    /// Action that will be revalidated before recovery begins.
    pub action: RecoveryAction,
}

/// One entry found while scanning an R library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryEntry {
    /// A valid installed package.
    Installed(InstalledPackage),
    /// A package-like directory that failed validation.
    Incomplete {
        /// Path to the invalid entry.
        path: PathBuf,
        /// Validation failure reported for the entry.
        reason: String,
    },
    /// A foreign or ambiguous lock that cannot be recovered automatically.
    Locked(LockMetadata),
    /// An installer-owned interrupted transaction with a recovery action.
    Recoverable(RecoveryPlan),
    /// A non-package filesystem entry.
    Foreign(PathBuf),
}
