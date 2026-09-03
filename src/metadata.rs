use std::{fs, path::Path};

use r_description::Description;

use crate::{Error, ExpectedPackage, PackageMetadata, Result, error::IoContext};

pub(crate) fn validate_package_tree(
    root: &Path,
    expected: &ExpectedPackage,
) -> Result<PackageMetadata> {
    let description_path = root.join("DESCRIPTION");
    let bytes = fs::read(&description_path).at(&description_path)?;
    let description = Description::parse_utf8(&bytes).map_err(|error| Error::InvalidMetadata {
        path: description_path.clone(),
        message: error.to_string(),
    })?;

    if let Some(diagnostic) = description.diagnostics().first() {
        return Err(Error::InvalidMetadata {
            path: description_path,
            message: diagnostic.message().to_owned(),
        });
    }
    if description.records().count() != 1 {
        return Err(Error::InvalidMetadata {
            path: description_path,
            message: "DESCRIPTION must contain exactly one DCF record".to_owned(),
        });
    }

    let name = required(&description, "Package", root)?;
    if name != expected.name {
        return Err(Error::PackageMismatch {
            expected: expected.name.clone(),
            actual: name,
        });
    }
    if root.file_name().and_then(|value| value.to_str()) != Some(expected.name.as_str()) {
        return Err(Error::InvalidMetadata {
            path: root.to_owned(),
            message: "package directory does not match Package".to_owned(),
        });
    }

    let version = required(&description, "Version", root)?;
    if description
        .version_parsed()
        .is_some_and(|value| value.is_err())
    {
        return Err(Error::InvalidMetadata {
            path: root.join("DESCRIPTION"),
            message: "invalid Version field".to_owned(),
        });
    }
    if version != expected.version {
        return Err(Error::VersionMismatch {
            expected: expected.version.clone(),
            actual: version,
        });
    }

    let built = required(&description, "Built", root)?;
    validate_built(&built, expected)?;

    let os_type = description.os_type().map(|value| value.as_str().to_owned());
    if let Some(value) = &os_type {
        let host = std::env::consts::OS;
        if (value == "windows" && host != "windows") || (value == "unix" && host == "windows") {
            return Err(Error::Incompatible(format!(
                "OS_type {value} is incompatible with {host}"
            )));
        }
    }

    validate_rds_marker(root)?;

    Ok(PackageMetadata {
        name: expected.name.clone(),
        version: expected.version.clone(),
        built,
        os_type,
        archs: description.archs().map(|value| value.as_str().to_owned()),
    })
}

pub(crate) fn inspect_package_tree(root: &Path) -> Result<PackageMetadata> {
    let description_path = root.join("DESCRIPTION");
    let bytes = fs::read(&description_path).at(&description_path)?;
    let description = Description::parse_utf8(&bytes).map_err(|error| Error::InvalidMetadata {
        path: description_path.clone(),
        message: error.to_string(),
    })?;
    if let Some(diagnostic) = description.diagnostics().first() {
        return Err(Error::InvalidMetadata {
            path: description_path,
            message: diagnostic.message().to_owned(),
        });
    }
    if description.records().count() != 1 {
        return Err(Error::InvalidMetadata {
            path: root.join("DESCRIPTION"),
            message: "DESCRIPTION must contain exactly one DCF record".to_owned(),
        });
    }
    let name = required(&description, "Package", root)?;
    let version = required(&description, "Version", root)?;
    if description
        .version_parsed()
        .is_some_and(|value| value.is_err())
    {
        return Err(Error::InvalidMetadata {
            path: root.join("DESCRIPTION"),
            message: "invalid Version field".to_owned(),
        });
    }
    let built = required(&description, "Built", root)?;
    if root.file_name().and_then(|value| value.to_str()) != Some(name.as_str()) {
        return Err(Error::InvalidMetadata {
            path: root.to_owned(),
            message: "directory name does not match Package".to_owned(),
        });
    }
    validate_rds_marker(root)?;
    Ok(PackageMetadata {
        name,
        version,
        built,
        os_type: description.os_type().map(|value| value.as_str().to_owned()),
        archs: description.archs().map(|value| value.as_str().to_owned()),
    })
}

fn validate_rds_marker(root: &Path) -> Result<()> {
    let marker = root.join("Meta").join("package.rds");
    let metadata = fs::symlink_metadata(&marker).map_err(|source| Error::InvalidMetadata {
        path: marker.clone(),
        message: source.to_string(),
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() == 0 {
        return Err(Error::InvalidMetadata {
            path: marker,
            message: "installed-package marker is not a nonempty regular file".to_owned(),
        });
    }
    Ok(())
}

fn required(description: &Description, name: &str, root: &Path) -> Result<String> {
    description
        .field(name)
        .map(|field| field.value().as_str().trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::InvalidMetadata {
            path: root.join("DESCRIPTION"),
            message: format!("missing {name} field"),
        })
}

fn validate_built(built: &str, expected: &ExpectedPackage) -> Result<()> {
    let mut parts = built.split(';').map(str::trim);
    let r_version = parts
        .next()
        .and_then(|value| value.strip_prefix("R "))
        .ok_or_else(|| Error::Incompatible(format!("invalid Built field: {built}")))?;
    let platform = parts
        .next()
        .ok_or_else(|| Error::Incompatible(format!("invalid Built field: {built}")))?;

    if let Some(required) = &expected.r_major_minor {
        let actual = r_version.split('.').take(2).collect::<Vec<_>>().join(".");
        if &actual != required {
            return Err(Error::Incompatible(format!(
                "package was built with R {actual}, expected R {required}"
            )));
        }
    }
    if let Some(required) = &expected.platform
        && platform != required
    {
        return Err(Error::Incompatible(format!(
            "package platform is {platform}, expected {required}"
        )));
    }
    if let Some(required) = &expected.architecture
        && !platform.contains(required)
    {
        return Err(Error::Incompatible(format!(
            "package platform {platform} does not contain architecture {required}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn validates_an_installed_tree_with_continuations() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("example");
        fs::create_dir_all(root.join("Meta")).unwrap();
        fs::write(
            root.join("DESCRIPTION"),
            "Package: example\nVersion: 1.2-3\nBuilt: R 4.5.2; aarch64-apple-darwin20; 2026-01-01; unix\nDescription: first\n second\n",
        )
        .unwrap();
        fs::write(root.join("Meta/package.rds"), b"rds").unwrap();
        let expected = ExpectedPackage {
            name: "example".into(),
            version: "1.2-3".into(),
            r_major_minor: Some("4.5".into()),
            platform: None,
            architecture: Some("aarch64".into()),
        };
        assert_eq!(
            validate_package_tree(&root, &expected).unwrap().name,
            "example"
        );
    }
}
