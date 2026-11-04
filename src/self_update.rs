//! Replace the standalone executable without changing binding or startup settings.
//! The caller must hold the profile lock and ensure the Agent is stopped.
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(not(all(windows, feature = "experimental-windows-processes")))]
use std::process::Stdio;

use sha2::{Digest, Sha256};
#[cfg(not(all(windows, feature = "experimental-windows-processes")))]
use tokio::io::AsyncReadExt;
use url::Url;

use crate::{
    error::{Error, Result},
    network::Network,
    update::{MAX_BINARY_BYTES, compare_versions, valid_version},
};

const RELEASE_API: &str =
    "https://api.github.com/repos/easelian/remote-device-agent/releases?per_page=1";
const RELEASE_BASE: &str = "https://github.com/easelian/remote-device-agent/releases/download";

fn platform() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return Err(Error::message("No release supports this CPU architecture")),
    };
    let platform = match std::env::consts::OS {
        "linux" => format!(
            "linux-{arch}-{}",
            if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            }
        ),
        "macos" => format!("darwin-{arch}"),
        "windows" => format!("win32-{arch}"),
        _ => return Err(Error::message("No release supports this operating system")),
    };
    Ok(platform)
}

fn release_version(bytes: &[u8]) -> Result<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let release = value
        .as_array()
        .and_then(|v| v.first())
        .filter(|v| v.get("draft").and_then(|v| v.as_bool()) == Some(false))
        .ok_or_else(|| Error::message("No published release is available"))?;
    let version = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .and_then(|v| v.strip_prefix('v'))
        .filter(|v| valid_version(v))
        .ok_or_else(|| Error::message("Published release has an invalid version tag"))?;
    Ok(version.to_owned())
}

fn checksum(bytes: &[u8], name: &str) -> Result<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::message("Invalid checksum record"))?;
    let mut selected = None;
    for line in text.lines() {
        let Some((digest, file)) = line.split_once("  ") else {
            return Err(Error::message("Invalid checksum record"));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::message("Invalid checksum record"));
        }
        if file == name && selected.replace(digest.to_owned()).is_some() {
            return Err(Error::message("Duplicate checksum record"));
        }
    }
    selected.ok_or_else(|| Error::message("Release has no checksum for this platform"))
}

struct Candidate(PathBuf);
impl Drop for Candidate {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn stage(target: &Path, bytes: &[u8], expected: &str) -> Result<Candidate> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_BINARY_BYTES {
        return Err(Error::message("Update executable has an invalid size"));
    }
    if format!("{:x}", Sha256::digest(bytes)) != expected {
        return Err(Error::message("Update executable checksum mismatch"));
    }
    let parent = target
        .parent()
        .ok_or_else(|| Error::message("Executable has no parent directory"))?;
    let path = parent.join(format!(
        ".rdh-update-{}{}",
        crate::random::hex_token::<16>()?,
        if cfg!(windows) { ".exe" } else { "" }
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    let mut file = options.open(&path)?;
    let candidate = Candidate(path);
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(candidate)
}

#[cfg(all(windows, feature = "experimental-windows-processes"))]
async fn validate_version(candidate: &Path, version: &str) -> Result<()> {
    let candidate = candidate.to_owned();
    let version = version.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::processes::check_update_program(
            &candidate,
            &["--version"],
            &version,
            false,
            Duration::from_secs(15),
        )
    })
    .await
    .map_err(|error| Error::message(error.to_string()))??;
    Ok(())
}

#[cfg(not(all(windows, feature = "experimental-windows-processes")))]
async fn validate_version(candidate: &Path, version: &str) -> Result<()> {
    let mut child = tokio::process::Command::new(candidate)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout").take(1025);
    let check = async {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        if bytes.len() > 1024 {
            return Err(Error::message(
                "Update executable version output is too large",
            ));
        }
        let status = child.wait().await?;
        if !status.success() || std::str::from_utf8(&bytes).ok().map(str::trim) != Some(version) {
            return Err(Error::message(
                "Update executable version mismatch or self-check failed",
            ));
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(15), check)
        .await
        .map_err(|_| Error::message("Update executable self-check timed out"))?
}

fn replace(candidate: &Path, target: &Path) -> Result<()> {
    // Never replace a directory or follow a newly substituted symlink.
    if !fs::symlink_metadata(target)?.file_type().is_file() {
        return Err(Error::message(
            "The installed executable is not a regular file",
        ));
    }
    #[cfg(unix)]
    replace_unix(candidate, target, crate::storage::sync_directory)?;
    #[cfg(windows)]
    replace_windows(candidate, target)?;
    #[cfg(not(any(unix, windows)))]
    return Err(Error::message("Executable replacement is unsupported"));
    Ok(())
}

#[cfg(unix)]
fn replace_unix(
    candidate: &Path,
    target: &Path,
    mut sync_directory: impl FnMut(&fs::File) -> std::io::Result<()>,
) -> Result<()> {
    fs::set_permissions(candidate, fs::metadata(target)?.permissions())?;
    fs::File::open(candidate)?.sync_all()?;
    let directory = fs::File::open(target.parent().unwrap())?;
    // Keep the original reachable until the replacement directory entry is
    // durable. Neither publication nor restoration ever removes target first.
    let backup = target.with_file_name(format!(
        ".rdh-previous-{}",
        crate::random::hex_token::<16>()?
    ));
    fs::hard_link(target, &backup)?;
    if let Err(error) = sync_directory(&directory).and_then(|()| fs::rename(candidate, target)) {
        let _ = fs::remove_file(&backup);
        return Err(error.into());
    }
    if let Err(error) = sync_directory(&directory) {
        if let Err(restore) = fs::rename(&backup, target) {
            return Err(Error::message(format!(
                "Update durability check failed ({error}); restoration failed ({restore}). Original executable is preserved at {}",
                backup.display()
            )));
        }
        let durability = sync_directory(&directory);
        return Err(Error::message(format!(
            "Update durability check failed ({error}); original executable restored (directory sync: {durability:?})"
        )));
    }
    if let Err(error) = fs::remove_file(&backup) {
        eprintln!(
            "Updated executable is durable; temporary file {} could not be removed: {error}",
            backup.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn replace_windows(candidate: &Path, target: &Path) -> Result<()> {
    use std::{
        mem::{offset_of, size_of},
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::{
        Wdk::Storage::FileSystem::{FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS},
        Win32::Storage::FileSystem::{
            DELETE, FILE_RENAME_INFO, FileRenameInfoEx, SetFileInformationByHandle,
        },
    };
    // One kernel rename replaces the directory entry while the running image
    // remains mapped. Never move the old executable out of the startup path.
    // An unsupported filesystem/OS fails safely; there is no two-rename fallback.
    let file = fs::OpenOptions::new().access_mode(DELETE).open(candidate)?;
    let name: Vec<u16> = target.as_os_str().encode_wide().collect();
    if name.len() > 32767 || name.contains(&0) {
        return Err(Error::message("Invalid executable replacement path"));
    }
    let length = size_of::<FILE_RENAME_INFO>() + name.len() * size_of::<u16>();
    let mut buffer = vec![0usize; length.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: usize aligns FILE_RENAME_INFO on supported Windows targets; the
    // zeroed allocation includes its complete header and the UTF-16 name. The
    // owned handle, allocation and name remain valid throughout the OS call.
    let renamed = unsafe {
        (*info).Anonymous.Flags = FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
        (*info).FileNameLength = (name.len() * size_of::<u16>()) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            info.cast::<u8>()
                .add(offset_of!(FILE_RENAME_INFO, FileName))
                .cast::<u16>(),
            name.len(),
        );
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileRenameInfoEx,
            info.cast(),
            length as u32,
        )
    };
    if renamed == 0 {
        return Err(Error::message(format!(
            "Atomic executable replacement is unavailable or was denied; the installed executable is unchanged: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Install the newest published release, including prereleases, as the bootstrap
/// installers do. No configuration, credentials or startup entries are modified.
pub async fn run() -> Result<()> {
    let platform = platform()?;
    let network = Network::from_env()?;
    let version = release_version(
        &network
            .get_update(&Url::parse(RELEASE_API).unwrap(), 1024 * 1024)
            .await?,
    )?;
    if compare_versions(&version, env!("CARGO_PKG_VERSION"))? != std::cmp::Ordering::Greater {
        println!(
            "Already running {}; no update needed.",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    }
    let name = format!(
        "rdh-{version}-{platform}{}",
        if cfg!(windows) { ".exe" } else { "" }
    );
    let base = format!("{RELEASE_BASE}/v{version}");
    let sums = network
        .get_update(&Url::parse(&format!("{base}/SHA256SUMS")).unwrap(), 65536)
        .await?;
    let expected = checksum(&sums, &name)?;
    let bytes = network
        .get_update(
            &Url::parse(&format!("{base}/{name}")).unwrap(),
            MAX_BINARY_BYTES as usize,
        )
        .await?;
    let target = std::env::current_exe()?.canonicalize()?;
    let candidate = stage(&target, &bytes, &expected)?;
    validate_version(&candidate.0, &version).await?;
    replace(&candidate.0, &target)?;
    println!("Updated to {version}. Run rdh to start the Agent.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_checksum_validation() {
        assert_eq!(
            release_version(br#"[{"tag_name":"v1.2.3-rc.1","draft":false}]"#).unwrap(),
            "1.2.3-rc.1"
        );
        for value in [
            br#"[]"#.as_slice(),
            br#"[{"tag_name":"v1.2.3","draft":true}]"#,
            br#"[{"tag_name":"v../bad","draft":false}]"#,
        ] {
            assert!(release_version(value).is_err());
        }
        let record = format!("{}  rdh\n", "a".repeat(64));
        assert!(checksum(record.as_bytes(), "other").is_err());
        assert!(checksum(format!("{record}{record}").as_bytes(), "rdh").is_err());
        assert_eq!(checksum(record.as_bytes(), "rdh").unwrap(), "a".repeat(64));
    }

    #[tokio::test]
    async fn failed_validation_preserves_installed_executable() {
        let dir = std::env::temp_dir().join(format!(
            "rdh-update-test-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("rdh");
        fs::write(&target, b"original").unwrap();
        assert!(stage(&target, b"candidate", &"0".repeat(64)).is_err());
        let bytes = b"not an executable";
        let candidate = stage(&target, bytes, &format!("{:x}", Sha256::digest(bytes))).unwrap();
        assert!(validate_version(&candidate.0, "1.2.3").await.is_err());
        drop(candidate);
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert!(replace(&dir.join("missing"), &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacement_installs_validated_candidate() {
        let dir = std::env::temp_dir().join(format!(
            "rdh-replace-test-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("rdh");
        fs::write(&target, b"original").unwrap();
        let bytes = b"replacement";
        let candidate = stage(&target, bytes, &format!("{:x}", Sha256::digest(bytes))).unwrap();
        replace(&candidate.0, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        drop(candidate);
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn version_mismatch_leaves_binding_and_executable_unchanged() {
        let dir = std::env::temp_dir().join(format!(
            "rdh-version-test-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("rdh");
        fs::write(&target, b"original").unwrap();
        fs::write(dir.join("binding.json"), b"preserve binding").unwrap();
        let bytes = b"#!/bin/sh\nprintf '1.2.3\\n'\n";
        let candidate = stage(&target, bytes, &format!("{:x}", Sha256::digest(bytes))).unwrap();
        assert!(validate_version(&candidate.0, "1.2.4").await.is_err());
        validate_version(&candidate.0, "1.2.3").await.unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(
            fs::read(dir.join("binding.json")).unwrap(),
            b"preserve binding"
        );
        drop(candidate);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_directory_sync_restores_original_without_touching_settings() {
        let dir = std::env::temp_dir().join(format!(
            "rdh-sync-test-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("rdh");
        fs::write(&target, b"original").unwrap();
        fs::write(dir.join("binding.json"), b"binding").unwrap();
        fs::write(dir.join("startup"), b"startup").unwrap();
        let bytes = b"replacement";
        let candidate = stage(&target, bytes, &format!("{:x}", Sha256::digest(bytes))).unwrap();
        let mut calls = 0;
        let result = replace_unix(&candidate.0, &target, |directory| {
            calls += 1;
            if calls == 2 {
                Err(std::io::Error::other("injected directory sync failure"))
            } else {
                crate::storage::sync_directory(directory)
            }
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("original executable restored")
        );
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read(dir.join("binding.json")).unwrap(), b"binding");
        assert_eq!(fs::read(dir.join("startup")).unwrap(), b"startup");
        drop(candidate);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn denied_windows_replacement_preserves_original_and_settings() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        let dir = std::env::temp_dir().join(format!(
            "rdh-denied-test-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let target = dir.join("rdh.exe");
        fs::write(&target, b"original").unwrap();
        fs::write(dir.join("binding.json"), b"binding").unwrap();
        fs::write(dir.join("startup"), b"startup").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&target)
            .unwrap();
        let bytes = b"replacement";
        let candidate = stage(&target, bytes, &format!("{:x}", Sha256::digest(bytes))).unwrap();
        assert!(replace(&candidate.0, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read(dir.join("binding.json")).unwrap(), b"binding");
        assert_eq!(fs::read(dir.join("startup")).unwrap(), b"startup");
        drop(held);
        drop(candidate);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 3);
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn replaces_running_windows_image_in_disposable_copy() {
        let dir = std::env::temp_dir().join(format!(
            "rdh-running-test-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&dir).unwrap();
        let copy = dir.join("rdh-test.exe");
        fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
        let filter = format!(
            "{}::running_image_replacement_child",
            module_path!().split_once("::").unwrap().1
        );
        let output = std::process::Command::new(&copy)
            .args(["--exact", &filter, "--ignored"])
            .env("RDH_TEST_REPLACE_COPY", &copy)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(dir.join("replacement-confirmed").exists());
        assert!(copy.is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "helper invoked only against an isolated copied test executable"]
    fn running_image_replacement_child() {
        let Some(copy) = std::env::var_os("RDH_TEST_REPLACE_COPY") else {
            return;
        };
        let target = PathBuf::from(copy).canonicalize().unwrap();
        assert_eq!(
            target,
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
        let bytes = fs::read(&target).unwrap();
        let candidate = stage(&target, &bytes, &format!("{:x}", Sha256::digest(&bytes))).unwrap();
        replace(&candidate.0, &target).unwrap();
        fs::write(
            target.parent().unwrap().join("replacement-confirmed"),
            b"ok",
        )
        .unwrap();
    }
}
