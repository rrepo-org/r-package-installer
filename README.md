# r-package-installer

Transactional installation of predownloaded R package artifacts.

[API documentation](https://docs.rs/r-package-installer) | [crates.io](https://crates.io/crates/r-package-installer)

This crate intentionally does not download packages, parse repositories,
resolve dependencies, or schedule package installation. A call handles one
artifact; callers may safely schedule independent calls from multiple threads.

## Boundary

- Binary ZIP and gzip-tar archives are safely extracted and validated in Rust.
- Source archives are installed by `R CMD INSTALL` into a private cache build
  library. Rust does not reproduce any R source-install behavior.
- Complete package trees are published into an immutable caller-owned cache.
- Cached trees are transactionally materialized into a target R library using
  R's `00LOCK-<package>/00new/<package>` layout.
- Installed libraries can be scanned, recovered, and modified through
  transactional removal.

`DESCRIPTION` files are handled by `r-description-parser`. Installer-owned DCF
metadata is handled by `r-dcf-syntax`.

## Usage

Add the crate to your project:

```toml
[dependencies]
r-package-installer = "0.1"
```

The caller downloads the artifact, computes its SHA-256 digest, and supplies
an authoritative package identity and cache key:

```rust,no_run
use std::path::PathBuf;

use r_package_installer::{
    Artifact, BinaryArtifact, BinaryFormat, CacheKey, Digest, ExpectedPackage,
    Installer, PrepareRequest,
};

let artifact_digest = Digest::from_hex(
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
)?;
let request = PrepareRequest {
    key: CacheKey::from_digest(artifact_digest),
    artifact_digest,
    expected: ExpectedPackage {
        name: "example".into(),
        version: "1.0.0".into(),
        r_major_minor: None,
        platform: None,
        architecture: None,
    },
    artifact: Artifact::Binary(BinaryArtifact {
        path: PathBuf::from("example_1.0.0.zip"),
        format: BinaryFormat::Zip,
    }),
};

let installer = Installer::new("package-cache");
let outcome = installer.install(&request, std::path::Path::new("R/library"))?;
# Ok::<_, r_package_installer::Error>(outcome)
```

For source packages, use `Artifact::Source`. The source cache key must include
every build input relevant to the caller, such as the R version, toolchain,
dependency library state, environment, and configure arguments.

## Base R interoperability

The default package-lock policy interoperates with single-package
`R CMD INSTALL`, `--pkglock`, and package-locked parallel installs. Foreign R
locks are never removed. Base R operations using `--no-lock` cannot be made
safe, and global `00LOCK` does not participate in R's package-lock protocol.
`LockPolicy::StrictLibrary` takes both lock forms when whole-library
serialization is preferable.

## Recovery

`scan_library` reports installer-owned interrupted transactions as
`LibraryEntry::Recoverable`. Before passing a recovery plan to
`recover_package`, the caller must establish that the process recorded as the
lock owner is no longer running. PID reuse prevents the library from proving
abandonment portably. Foreign or ambiguous locks are reported but never
removed.

## Windows

Rename, removal, rollback, and cleanup retry transient access, sharing, and
lock violations for a bounded period. Persistent failures are reported as a
package-in-use condition. The installer never falls back to copying directly
into a visible final package directory. Successful Windows renames use
`MoveFileExW` with `MOVEFILE_WRITE_THROUGH`; source processes are assigned to a
job object so timeout and cancellation terminate owned descendants.

## Platform support

The implementation targets macOS, Linux, and Windows. Source installation
requires R 3.6 or newer and delegates all source build behavior to
`R CMD INSTALL`.

## License

MIT
