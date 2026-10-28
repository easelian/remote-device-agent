//! Recoverable, offline native version-pointer transactions on Linux.
//! The caller must hold the exclusive installation manager lock.

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use crate::error::{Error, Result};
    use crate::install_lock::{
        ManagerLock, PENDING, pending, pointer, record_profile, version_binary,
    };
    use crate::profile_lock::ProfileLocks;
    use crate::{random, storage, update};

    const VERSION_MANIFEST: &str = ".native-version.json";

    fn sha256(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn check_elf(path: &Path) -> Result<()> {
        let mut file = File::open(path)?;
        let mut header = [0u8; 20];
        file.read_exact(&mut header)?;
        if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
            return Err(Error::message(
                "Native update is not a supported ELF executable",
            ));
        }
        let machine = u16::from_le_bytes([header[18], header[19]]);
        let expected = match std::env::consts::ARCH {
            "x86_64" => 62,
            "aarch64" => 183,
            _ => return Err(Error::message("Native update architecture is unsupported")),
        };
        if machine != expected {
            return Err(Error::message(
                "Native update architecture does not match this Agent",
            ));
        }
        Ok(())
    }

    fn self_check(
        path: &Path,
        arguments: &[&str],
        expected: &str,
        use_local_environment: bool,
        timeout: Duration,
    ) -> Result<()> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(path);
        command
            .args(arguments)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if use_local_environment {
            // These values are needed by the sandbox and startup probes. Do
            // not pass proxy settings, tokens, or the rest of the environment.
            for key in ["PATH", "HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
        } else {
            command.env("PATH", "");
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        fn stop(child: &mut std::process::Child) {
            // The unreaped child pins the process-group number until wait.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut output = child
            .stdout
            .take()
            .ok_or_else(|| Error::message("Native binary self-check has no output"))?;
        let flags = unsafe { libc::fcntl(output.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                < 0
        {
            stop(&mut child);
            return Err(Error::message("Cannot read native binary self-check"));
        }
        let deadline = Instant::now() + timeout;
        let mut captured = Vec::new();
        let mut buf = [0u8; 1024];
        let mut status: Option<std::process::ExitStatus> = None;
        loop {
            loop {
                match output.read(&mut buf) {
                    Ok(0) => break,
                    Ok(count) if captured.len() + count <= 4096 => {
                        captured.extend_from_slice(&buf[..count])
                    }
                    Ok(_) => {
                        if status.is_none() {
                            stop(&mut child);
                        }
                        return Err(Error::message(
                            "Native binary self-check output is too large",
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        if status.is_none() {
                            stop(&mut child);
                        }
                        return Err(error.into());
                    }
                }
            }
            if let Some(status) = status {
                if !std::process::ExitStatus::success(&status)
                    || std::str::from_utf8(&captured).ok().map(str::trim) != Some(expected)
                {
                    return Err(Error::message("Native binary self-check failed"));
                }
                return Ok(());
            }
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    stop(&mut child);
                    return Err(error.into());
                }
            };
            if status.is_none() {
                if Instant::now() >= deadline {
                    stop(&mut child);
                    return Err(Error::message("Native binary self-check timed out"));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn check_version(path: &Path, version: &str) -> Result<()> {
        self_check(path, &["--version"], version, false, Duration::from_secs(5))
    }

    fn check_service_protocol(path: &Path) -> Result<()> {
        self_check(
            path,
            &["service-update-protocol"],
            "rdh-service-update-v1",
            false,
            Duration::from_secs(5),
        )
        .map_err(|_| Error::message("Native binary does not support service update trials"))
    }

    /// Check the downloaded candidate before disturbing the running service.
    /// Staging later verifies the same size, digest, architecture and version.
    pub fn preflight_service(
        controller: &crate::install_lock::ControllerLock,
        version: &str,
        bytes: &[u8],
        digest: &str,
    ) -> Result<()> {
        if !update::valid_version(version)
            || bytes.is_empty()
            || bytes.len() as u64 > update::MAX_BINARY_BYTES
            || sha256(bytes) != digest
        {
            return Err(Error::message("Native update size or checksum is invalid"));
        }
        let temporary = Stage::new(controller.prefix(), "service-preflight")?;
        write_binary(&temporary.path, bytes)?;
        let path = temporary.path.join("rdh");
        check_elf(&path)?;
        check_version(&path, version)?;
        check_service_protocol(&path)
    }

    pub fn verify_service_protocol(prefix: &Path, version: &str) -> Result<()> {
        check_service_protocol(&version_binary(prefix, version)?)
    }

    /// The returned locks must remain held until the pointer switch finishes.
    pub fn validate_profiles(manager: &ManagerLock, version: &str) -> Result<ProfileLocks> {
        let profiles = manager.profiles()?;
        for config in &profiles {
            storage::private_metadata(config, false).map_err(|_| {
                Error::message(format!(
                    "Cannot read registered native profile: {}",
                    config.display()
                ))
            })?;
        }
        let locks = ProfileLocks::acquire(&profiles)?;
        let binary = version_binary(&manager.prefix, version)?;
        for config in &profiles {
            let path = config
                .to_str()
                .ok_or_else(|| Error::message("Invalid native profile path"))?;
            self_check(
                &binary,
                &["validate-config", "--config", path],
                "configuration valid",
                true,
                Duration::from_secs(15),
            )
            .map_err(|_| {
                Error::message(format!(
                    "Native version cannot validate registered profile: {}",
                    config.display()
                ))
            })?;
        }
        Ok(locks)
    }

    fn stored_manifest(directory: &Path, version: &str, bytes: &[u8]) -> Result<()> {
        let value = json!({"schema":1,"version":version,"kind":"native-binary",
            "binary":{"sha256":sha256(bytes),"bytes":bytes.len()}});
        storage::write_private_new(
            &directory.join(VERSION_MANIFEST),
            &serde_json::to_vec(&value)?,
        )
    }
    pub fn verify_stored(prefix: &Path, version: &str) -> Result<()> {
        let path = version_binary(prefix, version)?;
        let directory = path
            .parent()
            .ok_or_else(|| Error::message("Invalid native version directory"))?;
        let manifest: Value =
            serde_json::from_slice(&storage::read_private(&directory.join(VERSION_MANIFEST))?)?;
        let expected = manifest["binary"]["sha256"]
            .as_str()
            .filter(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(|| Error::message("Invalid native version metadata"))?;
        let size = manifest["binary"]["bytes"]
            .as_u64()
            .filter(|size| *size > 0 && *size <= update::MAX_BINARY_BYTES)
            .ok_or_else(|| Error::message("Invalid native version metadata"))?;
        if manifest["schema"].as_u64() != Some(1)
            || manifest["kind"].as_str() != Some("native-binary")
            || manifest["version"].as_str() != Some(version)
        {
            return Err(Error::message("Invalid native version metadata"));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let metadata = file.metadata()?;
        let named = storage::private_metadata(&path, false)?;
        if metadata.dev() != named.dev()
            || metadata.ino() != named.ino()
            || metadata.nlink() != 1
            || metadata.len() != size
        {
            return Err(Error::message("Native version binary changed"));
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        if format!("{:x}", hash.finalize()) != expected {
            return Err(Error::message("Native version checksum mismatch"));
        }
        check_elf(&path)?;
        check_version(&path, version)
    }

    struct Stage {
        path: PathBuf,
        device: u64,
        inode: u64,
    }
    impl Stage {
        fn new(parent: &Path, label: &str) -> Result<Self> {
            let path = parent.join(format!(".{label}.{}.tmp", random::hex_token::<16>()?));
            let metadata = storage::create_private_dir(&path)?;
            Ok(Self {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
    }
    impl Drop for Stage {
        fn drop(&mut self) {
            if let Ok(metadata) = fs::symlink_metadata(&self.path) {
                if metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode
                {
                    let _ = fs::remove_dir_all(&self.path);
                }
            }
        }
    }
    fn rename_new(source: &Path, destination: &Path) -> Result<()> {
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| Error::message("Invalid installation path"))?;
        let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())
            .map_err(|_| Error::message("Invalid installation path"))?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                1u32,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    fn write_binary(directory: &Path, bytes: &[u8]) -> Result<()> {
        let path = directory.join("rdh");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o700) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }

    pub fn stage(manager: &ManagerLock, version: &str, bytes: &[u8], digest: &str) -> Result<()> {
        if !update::valid_version(version)
            || bytes.is_empty()
            || bytes.len() as u64 > update::MAX_BINARY_BYTES
            || sha256(bytes) != digest
        {
            return Err(Error::message("Native update size or checksum is invalid"));
        }
        let versions = manager.prefix.join("versions");
        let destination = versions.join(version);
        if destination.exists() {
            version_binary(&manager.prefix, version)?;
            let manifest: Value = serde_json::from_slice(&storage::read_private(
                &destination.join(VERSION_MANIFEST),
            )?)?;
            if manifest["binary"]["sha256"].as_str() == Some(digest)
                && manifest["binary"]["bytes"].as_u64() == Some(bytes.len() as u64)
            {
                verify_stored(&manager.prefix, version)?;
                return Ok(());
            }
            return Err(Error::message(
                "A different native version already exists; it was not overwritten",
            ));
        }
        let temporary = Stage::new(&versions, "native-version")?;
        write_binary(&temporary.path, bytes)?;
        stored_manifest(&temporary.path, version, bytes)?;
        check_elf(&temporary.path.join("rdh"))?;
        check_version(&temporary.path.join("rdh"), version)?;
        File::open(&temporary.path)?.sync_all()?;
        rename_new(&temporary.path, &destination)?;
        File::open(&versions)?.sync_all()?;
        Ok(())
    }

    fn install_inner(prefix: &Path, profile: Option<&Path>) -> Result<PathBuf> {
        if !prefix.is_absolute() {
            return Err(Error::message(
                "Native installation prefix must be absolute",
            ));
        }
        let parent = prefix
            .parent()
            .ok_or_else(|| Error::message("Invalid native installation prefix"))?;
        let name = prefix
            .file_name()
            .ok_or_else(|| Error::message("Invalid native installation prefix"))?;
        fs::create_dir_all(parent)?;
        let parent = parent.canonicalize()?;
        let destination = parent.join(name);
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(Error::message(
                    "Installation prefix already exists; existing installations are not overwritten or migrated",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let executable = std::env::current_exe()?.canonicalize()?;
        let file = File::open(&executable)?;
        let mut bytes = Vec::new();
        file.take(update::MAX_BINARY_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() as u64 > update::MAX_BINARY_BYTES {
            return Err(Error::message("Native binary size is invalid"));
        }
        check_elf(&executable)?;
        check_version(&executable, env!("CARGO_PKG_VERSION"))?;
        let temporary = Stage::new(&parent, "rdh-install")?;
        let versions = temporary.path.join("versions");
        storage::create_private_dir(&versions)?;
        let version = versions.join(env!("CARGO_PKG_VERSION"));
        storage::create_private_dir(&version)?;
        write_binary(&version, &bytes)?;
        stored_manifest(&version, env!("CARGO_PKG_VERSION"), &bytes)?;
        File::open(&version)?.sync_all()?;
        File::open(&versions)?.sync_all()?;
        storage::write_private_new(
            &temporary.path.join(".native-install.json"),
            br#"{"schema":1,"kind":"native-agent"}"#,
        )?;
        storage::write_private_new(
            &temporary.path.join("install-id"),
            format!("{}\n", random::hex_token::<16>()?).as_bytes(),
        )?;
        if let Some(profile) = profile {
            record_profile(&temporary.path, profile)?;
        }
        symlink(
            format!("versions/{}", env!("CARGO_PKG_VERSION")),
            temporary.path.join("current"),
        )?;
        File::open(&temporary.path)?.sync_all()?;
        rename_new(&temporary.path, &destination)?;
        File::open(&parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                Error::message(format!(
                    "Native installation was created at {}, but its parent directory could not be synchronized: {error}",
                    destination.display()
                ))
            })?;
        Ok(destination)
    }

    pub fn install(prefix: &Path) -> Result<PathBuf> {
        install_inner(prefix, None)
    }

    /// The caller holds the profile lock and has validated the configuration.
    pub(crate) fn install_for_locked_profile(prefix: &Path, profile: &Path) -> Result<PathBuf> {
        install_inner(prefix, Some(profile))
    }

    pub fn previous(prefix: &Path) -> Result<Option<String>> {
        match fs::symlink_metadata(prefix.join("previous")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Ok(Some(pointer(prefix, "previous")?))
            }
            Ok(_) => Err(Error::message(
                "The previous version pointer is not a symbolic link",
            )),
        }
    }
    struct TempLink {
        path: PathBuf,
        device: u64,
        inode: u64,
    }
    impl Drop for TempLink {
        fn drop(&mut self) {
            if let Ok(metadata) = fs::symlink_metadata(&self.path) {
                if metadata.file_type().is_symlink()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode
                {
                    let _ = fs::remove_file(&self.path);
                }
            }
        }
    }
    fn set_pointer(prefix: &Path, name: &str, version: Option<&str>) -> Result<()> {
        if name != "current" && name != "previous" {
            return Err(Error::message("Invalid native version pointer"));
        }
        let destination = prefix.join(name);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(Error::message(
                    "Native version pointer is not a symbolic link",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && name == "previous" => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(version) = version {
            version_binary(prefix, version)?;
            let temp_path = prefix.join(format!(".{name}.{}.tmp", random::hex_token::<16>()?));
            symlink(format!("versions/{version}"), &temp_path)?;
            let metadata = fs::symlink_metadata(&temp_path)?;
            let temporary = TempLink {
                path: temp_path,
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            fs::rename(&temporary.path, &destination)?;
            drop(temporary);
        } else {
            if name != "previous" {
                return Err(Error::message("Current version pointer cannot be removed"));
            }
            match fs::remove_file(&destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        File::open(prefix)?.sync_all()?;
        Ok(())
    }
    fn clear_pending(prefix: &Path) -> Result<()> {
        pending(prefix)?;
        fs::remove_file(prefix.join(PENDING))?;
        File::open(prefix)?.sync_all()?;
        Ok(())
    }

    pub fn recover(manager: &ManagerLock) -> Result<()> {
        let prefix = &manager.prefix;
        let value = pending(prefix)?;
        let old = value["old_current"].as_str().unwrap();
        let new = value["new_current"].as_str().unwrap();
        let prior = value["old_previous"].as_str();
        version_binary(prefix, old)?;
        if let Some(prior) = prior {
            version_binary(prefix, prior)?;
        }
        let current = pointer(prefix, "current")?;
        if current != old && current != new {
            return Err(Error::message(
                "Current pointer is unrelated to the interrupted update; inspect before recovery",
            ));
        }
        let actual_previous = previous(prefix)?;
        if actual_previous.as_deref() != prior && actual_previous.as_deref() != Some(old) {
            return Err(Error::message(
                "Previous pointer changed during the interrupted update; inspect before recovery",
            ));
        }
        set_pointer(prefix, "previous", prior)?;
        set_pointer(prefix, "current", Some(old))?;
        clear_pending(prefix)
    }

    pub fn switch(manager: &ManagerLock, target: &str) -> Result<()> {
        if !update::valid_version(target) {
            return Err(Error::message("Invalid native target version"));
        }
        let prefix = &manager.prefix;
        let old = pointer(prefix, "current")?;
        if old == target {
            return Err(Error::message("The target version is already current"));
        }
        version_binary(prefix, &old)?;
        version_binary(prefix, target)?;
        let prior = previous(prefix)?;
        if let Some(prior) = &prior {
            version_binary(prefix, prior)?;
        }
        let value = json!({"schema":1,"old_current":old,"new_current":target,"old_previous":prior});
        storage::write_private_new(&prefix.join(PENDING), &serde_json::to_vec(&value)?)?;
        let outcome = (|| -> Result<()> {
            set_pointer(prefix, "previous", Some(&old))?;
            set_pointer(prefix, "current", Some(target))?;
            Ok(())
        })();
        if let Err(error) = outcome {
            return match recover(manager) {
                Ok(()) => Err(Error::message(format!(
                    "Native switch failed and was restored: {error}"
                ))),
                Err(_) => Err(Error::message(
                    "Native switch was interrupted; keep the installation stopped and run recovery",
                )),
            };
        }
        clear_pending(prefix).map_err(|_| Error::message("Native switch completed but its durable state is uncertain; inspect before starting"))
    }

    /// Restore the original pointers while the service transaction itself is
    /// the durable journal. The launch rule permits only the old version after
    /// the current pointer has reached it; a crash between these fsyncs remains
    /// recoverable without exposing an unverified new instance.
    pub fn restore_service(
        manager: &ManagerLock,
        expected: &crate::service_update::Intent,
    ) -> Result<()> {
        use crate::service_update::{Phase, read};
        if !manager.owns_service(&expected.transaction_id)
            || expected.phase != Phase::RollbackTrial
            || read(&manager.prefix)?.as_ref() != Some(expected)
        {
            return Err(Error::message("Service rollback ownership changed"));
        }
        let prefix = &manager.prefix;
        let current = pointer(prefix, "current")?;
        let actual_previous = previous(prefix)?;
        if (current != expected.old_current && current != expected.new_current)
            || (actual_previous.as_deref() != expected.old_previous.as_deref()
                && actual_previous.as_deref() != Some(expected.old_current.as_str()))
        {
            return Err(Error::message(
                "Installation pointers changed outside the service update",
            ));
        }
        version_binary(prefix, &expected.old_current)?;
        if let Some(prior) = &expected.old_previous {
            version_binary(prefix, prior)?;
        }
        set_pointer(prefix, "previous", expected.old_previous.as_deref())?;
        set_pointer(prefix, "current", Some(&expected.old_current))?;
        Ok(())
    }

    pub fn pending_record(prefix: &Path) -> Result<Value> {
        pending(prefix)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        struct Fixture {
            prefix: PathBuf,
            executable: PathBuf,
        }
        impl Fixture {
            fn new() -> Self {
                let prefix = std::env::temp_dir()
                    .join(format!("rdh-pointer-{}", random::hex_token::<8>().unwrap()));
                fs::create_dir(&prefix).unwrap();
                fs::set_permissions(&prefix, fs::Permissions::from_mode(0o700)).unwrap();
                fs::create_dir(prefix.join("versions")).unwrap();
                fs::set_permissions(prefix.join("versions"), fs::Permissions::from_mode(0o700))
                    .unwrap();
                for version in ["0.9.0", "1.0.0", "1.1.0", "2.0.0"] {
                    let directory = prefix.join("versions").join(version);
                    fs::create_dir(&directory).unwrap();
                    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
                    fs::write(directory.join("rdh"), version).unwrap();
                    fs::set_permissions(directory.join("rdh"), fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                symlink("versions/1.0.0", prefix.join("current")).unwrap();
                symlink("versions/0.9.0", prefix.join("previous")).unwrap();
                storage::write_private_new(
                    &prefix.join(".native-install.json"),
                    br#"{"schema":1,"kind":"native-agent"}"#,
                )
                .unwrap();
                let executable = prefix.join("versions/1.0.0/rdh");
                Self { prefix, executable }
            }
            fn pending(&self) {
                let record = json!({"schema":1,"old_current":"1.0.0","new_current":"1.1.0","old_previous":"0.9.0"});
                storage::write_private_new(
                    &self.prefix.join(PENDING),
                    &serde_json::to_vec(&record).unwrap(),
                )
                .unwrap();
            }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.prefix);
            }
        }

        #[test]
        fn binary_self_checks_bound_output_and_execution_time() {
            let fixture = Fixture::new();
            let helper = fixture.prefix.join("self-check");
            fs::write(
                &helper,
                b"#!/bin/sh\ni=0; while [ \"$i\" -lt 5000 ]; do printf x; i=$((i+1)); done\n",
            )
            .unwrap();
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(self_check(&helper, &[], "ok", false, Duration::from_secs(3)).is_err());
            fs::write(&helper, b"#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
            let start = Instant::now();
            assert!(self_check(&helper, &[], "ok", false, Duration::from_millis(100)).is_err());
            assert!(start.elapsed() < Duration::from_secs(3));
        }

        #[test]
        fn offline_switch_and_rollback_swap_the_two_native_pointers() {
            let fixture = Fixture::new();
            let manager =
                ManagerLock::acquire(&fixture.prefix, &fixture.executable, false).unwrap();
            switch(&manager, "1.1.0").unwrap();
            assert_eq!(pointer(&fixture.prefix, "current").unwrap(), "1.1.0");
            assert_eq!(pointer(&fixture.prefix, "previous").unwrap(), "1.0.0");
            assert!(!fixture.prefix.join(PENDING).exists());
            switch(&manager, "1.0.0").unwrap();
            assert_eq!(pointer(&fixture.prefix, "current").unwrap(), "1.0.0");
            assert_eq!(pointer(&fixture.prefix, "previous").unwrap(), "1.1.0");
        }

        #[test]
        fn every_interrupted_switch_boundary_recovers_old_pointers() {
            for stage in 0..3 {
                let fixture = Fixture::new();
                fixture.pending();
                if stage >= 1 {
                    set_pointer(&fixture.prefix, "previous", Some("1.0.0")).unwrap();
                }
                if stage >= 2 {
                    set_pointer(&fixture.prefix, "current", Some("1.1.0")).unwrap();
                }
                let executable = if stage == 2 {
                    fixture.prefix.join("versions/1.1.0/rdh")
                } else {
                    fixture.executable.clone()
                };
                let manager = ManagerLock::acquire(&fixture.prefix, &executable, true).unwrap();
                recover(&manager).unwrap();
                assert_eq!(pointer(&fixture.prefix, "current").unwrap(), "1.0.0");
                assert_eq!(pointer(&fixture.prefix, "previous").unwrap(), "0.9.0");
                assert!(!fixture.prefix.join(PENDING).exists());
            }
        }

        #[test]
        fn recovery_refuses_to_overwrite_an_unrelated_pointer() {
            let fixture = Fixture::new();
            fixture.pending();
            set_pointer(&fixture.prefix, "current", Some("2.0.0")).unwrap();
            let manager = ManagerLock::acquire(&fixture.prefix, &fixture.executable, true).unwrap();
            assert!(recover(&manager).is_err());
            assert_eq!(pointer(&fixture.prefix, "current").unwrap(), "2.0.0");
            assert!(fixture.prefix.join(PENDING).exists());
        }

        #[test]
        fn mismatched_existing_version_is_not_executed_or_overwritten() {
            let fixture = Fixture::new();
            let directory = fixture.prefix.join("versions/1.1.0");
            let evidence = fixture.prefix.join("executed");
            fs::write(
                directory.join("rdh"),
                format!("#!/bin/sh\ntouch '{}'\n", evidence.display()),
            )
            .unwrap();
            let manifest = json!({"schema":1,"version":"1.1.0","kind":"native-binary",
                "binary":{"sha256":"a".repeat(64),"bytes":5}});
            storage::write_private_new(
                &directory.join(VERSION_MANIFEST),
                &serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let manager =
                ManagerLock::acquire(&fixture.prefix, &fixture.executable, false).unwrap();
            let incoming = b"different native binary";
            assert!(stage(&manager, "1.1.0", incoming, &sha256(incoming)).is_err());
            assert!(!evidence.exists());
            assert_eq!(
                fs::read_link(fixture.prefix.join("current")).unwrap(),
                PathBuf::from("versions/1.0.0")
            );
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::install_for_locked_profile;
#[cfg(target_os = "linux")]
pub use linux::{
    install, pending_record, preflight_service, previous, recover, restore_service, stage, switch,
    validate_profiles, verify_service_protocol, verify_stored,
};
