# Changelog

All notable changes to this project are documented in this file.

## 0.2.0 - Unreleased

- Automatically detect ZIP, gzip-tar, and XZ-tar binary archives by magic bytes,
  including XZ-compressed macOS binaries named `.tgz` (RRE-97).
- Breaking: remove `BinaryFormat` and `BinaryArtifact::format`. Callers now
  construct `BinaryArtifact { path }` and remove `BinaryFormat` imports.
- Add `Error::UnsupportedArchiveFormat` for unrecognized signatures; recognized
  but corrupt or truncated archives return `InvalidArchive`.
- Validate gzip/XZ stream integrity through compression trailers under the
  existing decompression limits.

## 0.1.2 - 2026-09-14

- Remove byte-level private build-path rejection after source installation.
  Embedded paths can be harmless debug metadata, as in pak's bundled `zip.so`,
  and do not reliably indicate runtime dependencies on the build directory.

## 0.1.1 - 2026-09-04

- Accept PE images when validating native DLLs in Windows binary packages.
- Report native-library format mismatches separately from architecture
  mismatches.

## 0.1.0 - 2026-09-04

- Add hardened extraction and validation for binary R package archives.
- Add source package installation through an owned `R CMD INSTALL` process.
- Add immutable caller-keyed package caching with concurrent population.
- Add transactional package installation, replacement, removal, and recovery.
- Add package and strict-library locking compatible with base R conventions.
- Add macOS, Linux, and Windows support, including Windows job objects and
  write-through renames.
