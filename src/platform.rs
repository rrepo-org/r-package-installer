use std::{fs, path::Path};

use object::{
    Architecture, BinaryFormat, FileKind, Object as _,
    read::macho::{FatArch as _, MachOFatFile32, MachOFatFile64},
};

use crate::{Error, ExpectedPackage, Result, error::IoContext};

pub(crate) fn validate_native_code(root: &Path, expected: &ExpectedPackage) -> Result<()> {
    let architecture = if let Some(value) = expected.architecture.as_deref() {
        Some(expected_architecture(value).ok_or_else(|| {
            Error::Unsupported(format!("unsupported requested architecture {value}"))
        })?)
    } else {
        expected_architecture(std::env::consts::ARCH)
    };
    let Some(architecture) = architecture else {
        return Ok(());
    };
    for directory in [root.join("libs"), root.join("lib")] {
        if directory.is_dir() {
            scan(&directory, architecture)?;
        }
    }
    Ok(())
}

fn scan(directory: &Path, expected: Architecture) -> Result<()> {
    for entry in fs::read_dir(directory).at(directory)? {
        let entry = entry.at(directory)?;
        let path = entry.path();
        let file_type = entry.file_type().at(&path)?;
        if file_type.is_dir() {
            scan(&path, expected)?;
            continue;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase);
        if !file_type.is_file() || !matches!(extension.as_deref(), Some("so" | "dylib" | "dll")) {
            continue;
        }
        let bytes = fs::read(&path).at(&path)?;
        let kind = FileKind::parse(bytes.as_slice()).map_err(|error| {
            Error::Incompatible(format!(
                "cannot parse native library {}: {error}",
                path.display()
            ))
        })?;
        let matches = match kind {
            FileKind::MachOFat32 => MachOFatFile32::parse(bytes.as_slice())
                .map(|file| {
                    file.arches()
                        .iter()
                        .any(|arch| arch.architecture() == expected)
                })
                .unwrap_or(false),
            FileKind::MachOFat64 => MachOFatFile64::parse(bytes.as_slice())
                .map(|file| {
                    file.arches()
                        .iter()
                        .any(|arch| arch.architecture() == expected)
                })
                .unwrap_or(false),
            _ => object::File::parse(bytes.as_slice())
                .map(|file| file.architecture() == expected && format_matches_host(file.format()))
                .unwrap_or(false),
        };
        if !matches {
            return Err(Error::Incompatible(format!(
                "native library {} does not contain architecture {expected:?}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn format_matches_host(format: BinaryFormat) -> bool {
    match std::env::consts::OS {
        "macos" => format == BinaryFormat::MachO,
        "linux" => format == BinaryFormat::Elf,
        "windows" => format == BinaryFormat::Coff,
        _ => true,
    }
}

fn expected_architecture(value: &str) -> Option<Architecture> {
    match value.to_ascii_lowercase().as_str() {
        "aarch64" | "arm64" => Some(Architecture::Aarch64),
        "x86_64" | "x86-64" | "amd64" => Some(Architecture::X86_64),
        "x86" | "i386" | "i686" => Some(Architecture::I386),
        "arm" | "armv7" => Some(Architecture::Arm),
        "powerpc64" | "ppc64" => Some(Architecture::PowerPc64),
        _ => None,
    }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    use tempfile::TempDir;

    use super::*;

    const PE_DLL: &[u8] = include_bytes!("../tests/fixtures/x86_64-windows.dll");
    const COFF_OBJECT: &[u8] = include_bytes!("../tests/fixtures/x86_64-windows.obj");

    fn native_library(bytes: &[u8]) -> TempDir {
        let temp = TempDir::new().unwrap();
        let libs = temp.path().join("libs");
        fs::create_dir(&libs).unwrap();
        fs::write(libs.join("native.dll"), bytes).unwrap();
        temp
    }

    fn expected(architecture: &str) -> ExpectedPackage {
        ExpectedPackage {
            name: "native".into(),
            version: "1.0.0".into(),
            r_major_minor: None,
            platform: None,
            architecture: Some(architecture.into()),
        }
    }

    #[test]
    fn accepts_x86_64_pe_dll_on_windows() {
        let package = native_library(PE_DLL);

        validate_native_code(package.path(), &expected("x86_64")).unwrap();
    }

    #[test]
    fn reports_incompatible_pe_architecture() {
        let package = native_library(PE_DLL);

        let error = validate_native_code(package.path(), &expected("aarch64")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not contain architecture Aarch64")
        );
    }

    #[test]
    fn reports_coff_as_an_incompatible_windows_format() {
        let package = native_library(COFF_OBJECT);

        let error = validate_native_code(package.path(), &expected("x86_64")).unwrap_err();

        assert!(error.to_string().contains("format Coff, expected Pe"));
    }
}
