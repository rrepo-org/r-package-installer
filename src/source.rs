use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use command_group::{CommandGroup as _, GroupChild};
use r_description::Description;

use crate::{Error, ExpectedPackage, Result, SourceArtifact, error::IoContext};

const MAX_CAPTURED_OUTPUT: usize = 1024 * 1024;

pub(crate) fn materialize_source(
    artifact: &SourceArtifact,
    build_root: &Path,
    expected: &ExpectedPackage,
) -> Result<PathBuf> {
    let library = build_root.join("library");
    fs::create_dir_all(&library).at(&library)?;

    let mut command = Command::new(&artifact.options.r_executable);
    command
        .arg("CMD")
        .arg("INSTALL")
        .arg("--use-vanilla")
        .arg("--pkglock")
        .arg("--staged-install")
        .arg("-l")
        .arg(&library);
    if !artifact.options.configure_args.is_empty() {
        command.arg(format!(
            "--configure-args={}",
            artifact.options.configure_args.join(" ")
        ));
    }
    command
        .arg(&artifact.path)
        .current_dir(build_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.envs(&artifact.options.environment);
    if !artifact.options.dependency_libraries.is_empty() {
        let paths =
            std::env::join_paths(&artifact.options.dependency_libraries).map_err(|error| {
                Error::Unsupported(format!("invalid dependency library path: {error}"))
            })?;
        command.env("R_LIBS", paths);
    } else {
        command.env_remove("R_LIBS");
    }
    command.env("R_INSTALL_VANILLA", "1").env("MAKEFLAGS", "");
    let mut child = command.group_spawn().at(&artifact.options.r_executable)?;
    let stdout = child.inner().stdout.take().map(drain);
    let stderr = child.inner().stderr.take().map(drain);
    let started = Instant::now();
    let failure = loop {
        if let Some(status) = child.try_wait().at(&artifact.options.r_executable)? {
            break if status.success() {
                None
            } else {
                Some(Error::RInstallFailed {
                    status: status.code(),
                    output: String::new(),
                })
            };
        }
        if artifact.options.cancellation.is_cancelled() {
            terminate_tree(&mut child);
            break Some(Error::RInstallCancelled);
        }
        if started.elapsed() >= artifact.options.timeout {
            terminate_tree(&mut child);
            break Some(Error::RInstallTimeout);
        }
        thread::sleep(Duration::from_millis(100));
    };
    let _ = child.wait();
    let stdout = join_output(stdout);
    let stderr = join_output(stderr);
    if let Some(mut error) = failure {
        if let Error::RInstallFailed { output, .. } = &mut error {
            *output = combine_output(&stdout, &stderr);
        }
        return Err(error);
    }

    let package_root = library.join(&expected.name);
    if !package_root.is_dir() {
        return Err(Error::InvalidMetadata {
            path: package_root,
            message: "R CMD INSTALL did not produce the expected package".into(),
        });
    }
    if !artifact.options.allow_non_staged {
        let description_path = package_root.join("DESCRIPTION");
        let description_bytes = fs::read(&description_path).at(&description_path)?;
        let description = Description::parse_utf8(&description_bytes).map_err(|error| {
            Error::InvalidMetadata {
                path: description_path,
                message: error.to_string(),
            }
        })?;
        if description
            .staged_install()
            .is_some_and(|value| value.as_str().trim().eq_ignore_ascii_case("no"))
        {
            return Err(Error::Incompatible(
                "source package declares StagedInstall: no".into(),
            ));
        }
    }
    Ok(package_root)
}

fn drain(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        let mut chunk = [0_u8; 16 * 1024];
        while let Ok(count) = pipe.read(&mut chunk) {
            if count == 0 {
                break;
            }
            if output.len() + count > MAX_CAPTURED_OUTPUT {
                let discard = (output.len() + count) - MAX_CAPTURED_OUTPUT;
                if discard >= output.len() {
                    output.clear();
                } else {
                    output.drain(..discard);
                }
            }
            output.extend_from_slice(&chunk[..count]);
        }
        output
    })
}

fn join_output(handle: Option<thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
        (true, true) => String::new(),
        (true, false) => stderr.into_owned(),
        (false, true) => stdout.into_owned(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn terminate_tree(child: &mut GroupChild) {
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combines_both_r_output_streams() {
        assert_eq!(combine_output(b"out", b"err"), "out\nerr");
    }
}
