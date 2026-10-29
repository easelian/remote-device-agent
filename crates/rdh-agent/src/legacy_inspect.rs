//! Read-only inventory for a user-owned legacy Node installation.
//!
//! An inventory is a point-in-time observation, not permission to stop a
//! service or switch versions. Offline migration has its own stricter checks.

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CString, OsStr};
    use std::fs::{self, File, Metadata};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf};

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use crate::config::DeviceConfig;
    use crate::error::{Error, Result};
    use crate::{startup, systemd_identity, update};

    const USAGE: &str = "rdh migrate --inspect --prefix OLD_INSTALL --config ABSOLUTE_PATH [--service-unit NAME]\n\
        rdh migrate --offline|--undo --prefix OLD_INSTALL --config ABSOLUTE_PATH --service-unit NAME [--recover-lock]\n\
        Inspection is read-only. Offline migration requires a stopped user service and a prepared native installation.\n";

    #[derive(Default)]
    struct Options {
        inspect: bool,
        prefix: Option<PathBuf>,
        config: Option<PathBuf>,
        unit: Option<String>,
    }

    fn options(args: &[String]) -> Result<Options> {
        let mut value = Options::default();
        let mut items = args.iter();
        while let Some(arg) = items.next() {
            match arg.as_str() {
                "--inspect" if !value.inspect => value.inspect = true,
                "--prefix" | "--config" | "--service-unit" => {
                    let next = items
                        .next()
                        .filter(|item| !item.is_empty() && !item.starts_with("--"))
                        .ok_or_else(|| Error::message(format!("{arg} requires a value")))?;
                    match arg.as_str() {
                        "--prefix" if value.prefix.is_none() => value.prefix = Some(next.into()),
                        "--config" if value.config.is_none() => value.config = Some(next.into()),
                        "--service-unit" if value.unit.is_none() => value.unit = Some(next.clone()),
                        _ => return Err(Error::message("Duplicate migration option")),
                    }
                }
                _ => return Err(Error::message("Invalid migration option")),
            }
        }
        if !value.inspect
            || !value.prefix.as_deref().is_some_and(clean_absolute)
            || !value.config.as_deref().is_some_and(clean_absolute)
            || value
                .unit
                .as_deref()
                .is_some_and(|name| !systemd_identity::valid_unit(name))
        {
            return Err(Error::message(USAGE));
        }
        Ok(value)
    }

    fn clean_absolute(path: &Path) -> bool {
        path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
            && path.as_os_str().as_bytes().len() <= 4096
    }

    fn invalid() -> Error {
        Error::message("Legacy installation layout, ownership, or identity cannot be verified")
    }

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    fn open_prefix(path: &Path) -> Result<File> {
        let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid())?;
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: 0x04 | 0x02, // RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS
        };
        // SAFETY: name and how are live for the syscall; success returns a
        // uniquely owned descriptor. No symbolic-link component is followed.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                libc::AT_FDCWD,
                name.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    fn open_child(parent: &File, name: &OsStr, directory: bool) -> Result<File> {
        let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: parent and the single-component NUL-terminated name are live.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn trusted(metadata: &Metadata, directory: bool, executable: bool) -> Result<()> {
        // SAFETY: geteuid has no preconditions and does not mutate memory.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
            || (if directory {
                !metadata.is_dir()
            } else {
                !metadata.is_file() || metadata.nlink() != 1
            })
            || (executable && metadata.mode() & 0o100 == 0)
        {
            return Err(invalid());
        }
        Ok(())
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Signature {
        device: u64,
        inode: u64,
        length: u64,
        mode: u32,
        uid: u32,
        links: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    }

    fn signature(metadata: &Metadata) -> Signature {
        Signature {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            links: metadata.nlink(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }

    fn optional_signature(path: &Path) -> Result<Option<Signature>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => Ok(Some(signature(&metadata))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    struct StartupObservation {
        paths: [PathBuf; 3],
        signatures: [Option<Signature>; 3],
        value: Value,
    }

    impl StartupObservation {
        fn inspect(config: &Path, unit: Option<&str>) -> Result<Self> {
            let state = startup::sidecar_path(config);
            let append = |suffix: &str| {
                let mut path = state.as_os_str().to_os_string();
                path.push(suffix);
                PathBuf::from(path)
            };
            let paths = [
                state.clone(),
                append(".node-lock"),
                append(".node-lock.reclaiming"),
            ];
            let signatures = [
                optional_signature(&paths[0])?,
                optional_signature(&paths[1])?,
                optional_signature(&paths[2])?,
            ];
            let (status, binding, pending) = if signatures[0].is_none() {
                ("missing", None, None)
            } else {
                match startup::inspect_sidecar(config) {
                    Ok((binding, pending)) => ("readable", binding, pending),
                    Err(_) => ("unavailable", None, None),
                }
            };
            let matches_unit = unit.and_then(|unit| {
                binding
                    .as_ref()
                    .map(|binding| binding.mode == startup::Mode::User && binding.name == unit)
            });
            let value = json!({
                "state":status,
                "binding":binding,
                "binding_matches_user_unit":matches_unit,
                "pending_mode":pending,
                "state_lock_present":signatures[1].is_some(),
                "recovery_guard_present":signatures[2].is_some(),
            });
            Ok(Self {
                paths,
                signatures,
                value,
            })
        }

        fn recheck(&self) -> Result<()> {
            for (path, before) in self.paths.iter().zip(&self.signatures) {
                if &optional_signature(path)? != before {
                    return Err(Error::message("Startup state changed during inspection"));
                }
            }
            Ok(())
        }
    }

    fn current(prefix: &File) -> Result<String> {
        let mut data = vec![0u8; 4097];
        // SAFETY: prefix is a live directory descriptor, the literal is NUL
        // terminated, and data is a valid writable buffer.
        let length = unsafe {
            libc::readlinkat(
                prefix.as_raw_fd(),
                c"current".as_ptr(),
                data.as_mut_ptr().cast(),
                data.len(),
            )
        };
        if length <= 0 || length as usize >= data.len() {
            return Err(invalid());
        }
        data.truncate(length as usize);
        let target = Path::new(OsStr::from_bytes(&data));
        let mut parts = target.components();
        if parts.next() != Some(Component::Normal(OsStr::new("versions"))) {
            return Err(invalid());
        }
        let version = match parts.next() {
            Some(Component::Normal(name)) => name.to_str().filter(|v| update::valid_version(v)),
            _ => None,
        }
        .ok_or_else(invalid)?;
        if parts.next().is_some() || data != format!("versions/{version}").as_bytes() {
            return Err(invalid());
        }
        Ok(version.to_owned())
    }

    fn reject_native_marker(prefix: &File) -> Result<()> {
        // A native install is a different format, even if it happens to contain
        // a Node executable. The marker itself must not be followed.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: prefix and the fixed NUL-terminated name are live; stat is a
        // writable output which is not read on failure.
        let status = unsafe {
            libc::fstatat(
                prefix.as_raw_fd(),
                c".native-install.json".as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
            return Err(invalid());
        }
        Ok(())
    }

    struct ObservedFile {
        file: File,
        signature: Signature,
        digest: String,
        content: Option<Vec<u8>>,
        header: Vec<u8>,
    }

    fn observe_file(
        parent: &File,
        name: &str,
        max: u64,
        executable: bool,
        capture: bool,
    ) -> Result<ObservedFile> {
        let mut file = open_child(parent, OsStr::new(name), false)?;
        let before = file.metadata()?;
        trusted(&before, false, executable)?;
        if before.len() > max {
            return Err(invalid());
        }
        let mut digest = Sha256::new();
        let mut content = capture.then(Vec::new);
        let mut header = Vec::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total.checked_add(count as u64).ok_or_else(invalid)?;
            if total > max {
                return Err(invalid());
            }
            if header.len() < 20 {
                let size = count.min(20 - header.len());
                header.extend_from_slice(&buffer[..size]);
            }
            digest.update(&buffer[..count]);
            if let Some(bytes) = content.as_mut() {
                bytes.extend_from_slice(&buffer[..count]);
            }
        }
        let before = signature(&before);
        if total != before.length || signature(&file.metadata()?) != before {
            return Err(Error::message(
                "Legacy installation changed during inspection",
            ));
        }
        Ok(ObservedFile {
            file,
            signature: before,
            digest: format!("{:x}", digest.finalize()),
            content,
            header,
        })
    }

    fn same_named(parent: &File, name: &str, observed: &ObservedFile) -> Result<()> {
        let named = open_child(parent, OsStr::new(name), false)?;
        if signature(&named.metadata()?) != observed.signature
            || signature(&observed.file.metadata()?) != observed.signature
        {
            return Err(Error::message(
                "Legacy installation changed during inspection",
            ));
        }
        Ok(())
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct ScriptIdentity {
        parent_device: u64,
        parent_inode: u64,
        signature: Signature,
        digest: String,
    }

    /// Read one expected wrapper without following any path symlinks. The
    /// identity is compared across service snapshots and is never printed.
    pub(crate) fn observe_wrapper(parent: &Path, name: &str) -> Result<(Vec<u8>, ScriptIdentity)> {
        let mut components = Path::new(name).components();
        if !clean_absolute(parent)
            || name.as_bytes().contains(&b'/')
            || !matches!(components.next(), Some(Component::Normal(_)))
            || components.next().is_some()
        {
            return Err(invalid());
        }
        let directory = open_prefix(parent)?;
        let metadata = directory.metadata()?;
        trusted(&metadata, true, false)?;
        let observed = observe_file(&directory, name, 64 * 1024, true, true)?;
        same_named(&directory, name, &observed)?;
        let reopened = open_prefix(parent)?.metadata()?;
        trusted(&reopened, true, false)?;
        if metadata.dev() != reopened.dev() || metadata.ino() != reopened.ino() {
            return Err(Error::message("Legacy wrapper changed during inspection"));
        }
        Ok((
            observed.content.ok_or_else(invalid)?,
            ScriptIdentity {
                parent_device: metadata.dev(),
                parent_inode: metadata.ino(),
                signature: observed.signature,
                digest: observed.digest,
            },
        ))
    }

    struct Layout {
        path: PathBuf,
        prefix: File,
        versions: File,
        version_dir: File,
        bin: File,
        bundle_dir: File,
        version: String,
        node: ObservedFile,
        launcher: ObservedFile,
        entry: ObservedFile,
        bundle: ObservedFile,
        package: ObservedFile,
    }

    impl Layout {
        fn inspect(path: &Path) -> Result<Self> {
            let prefix = open_prefix(path)?;
            trusted(&prefix.metadata()?, true, false)?;
            reject_native_marker(&prefix)?;
            let version = current(&prefix)?;
            let versions = open_child(&prefix, OsStr::new("versions"), true)?;
            trusted(&versions.metadata()?, true, false)?;
            let version_dir = open_child(&versions, OsStr::new(&version), true)?;
            trusted(&version_dir.metadata()?, true, false)?;
            let bin = open_child(&version_dir, OsStr::new("bin"), true)?;
            trusted(&bin.metadata()?, true, false)?;
            let bundle_dir = open_child(&version_dir, OsStr::new("bundle"), true)?;
            trusted(&bundle_dir.metadata()?, true, false)?;
            let node = observe_file(&version_dir, "node", 256 * 1024 * 1024, true, false)?;
            let launcher = observe_file(&version_dir, "rdh", 1024 * 1024, true, true)?;
            let entry = observe_file(&bin, "remote-device-hub.mjs", 1024 * 1024, false, false)?;
            let bundle = observe_file(&bundle_dir, "agent.mjs", 64 * 1024 * 1024, false, false)?;
            let package = observe_file(&version_dir, "package.json", 1024 * 1024, false, true)?;
            let manifest: Value =
                serde_json::from_slice(package.content.as_deref().ok_or_else(invalid)?)?;
            if manifest["name"] != "remote-device-hub" || manifest["version"] != version {
                return Err(invalid());
            }
            let expected_machine = match std::env::consts::ARCH {
                "x86_64" => 62,
                "aarch64" => 183,
                _ => return Err(invalid()),
            };
            if node.header.len() < 20
                || &node.header[..4] != b"\x7fELF"
                || node.header[4..6] != [2, 1]
                || u16::from_le_bytes([node.header[18], node.header[19]]) != expected_machine
            {
                return Err(invalid());
            }
            if !launcher.header.starts_with(b"#!") {
                return Err(invalid());
            }
            let layout = Self {
                path: path.to_owned(),
                prefix,
                versions,
                version_dir,
                bin,
                bundle_dir,
                version,
                node,
                launcher,
                entry,
                bundle,
                package,
            };
            layout.recheck()?;
            Ok(layout)
        }

        fn recheck(&self) -> Result<()> {
            let prefix = open_prefix(&self.path)?;
            reject_native_marker(&prefix)?;
            let versions = open_child(&prefix, OsStr::new("versions"), true)?;
            let version = open_child(&versions, OsStr::new(&self.version), true)?;
            let bin = open_child(&version, OsStr::new("bin"), true)?;
            let bundle = open_child(&version, OsStr::new("bundle"), true)?;
            for (original, now) in [
                (&self.prefix, &prefix),
                (&self.versions, &versions),
                (&self.version_dir, &version),
                (&self.bin, &bin),
                (&self.bundle_dir, &bundle),
            ] {
                let a = original.metadata()?;
                let b = now.metadata()?;
                trusted(&b, true, false)?;
                if a.dev() != b.dev() || a.ino() != b.ino() {
                    return Err(Error::message(
                        "Legacy installation changed during inspection",
                    ));
                }
            }
            if current(&prefix)? != self.version {
                return Err(Error::message(
                    "Legacy installation changed during inspection",
                ));
            }
            same_named(&version, "node", &self.node)?;
            same_named(&version, "rdh", &self.launcher)?;
            same_named(&bin, "remote-device-hub.mjs", &self.entry)?;
            same_named(&bundle, "agent.mjs", &self.bundle)?;
            same_named(&version, "package.json", &self.package)
        }
    }

    /// Keep the opened legacy files alive while a migration controller checks
    /// the service definition. The evidence is not provenance verification.
    pub(crate) struct MigrationLayout(Layout);

    impl MigrationLayout {
        pub(crate) fn inspect(path: &Path) -> Result<Self> {
            Ok(Self(Layout::inspect(path)?))
        }
        pub(crate) fn node(&self) -> &File {
            &self.0.node.file
        }
        pub(crate) fn entry(&self) -> &File {
            &self.0.entry.file
        }
        pub(crate) fn launcher(&self) -> &[u8] {
            self.0.launcher.content.as_deref().unwrap_or_default()
        }
        pub(crate) fn evidence(&self) -> Value {
            json!({
                "version":self.0.version,
                "node":self.0.node.digest,
                "launcher":self.0.launcher.digest,
                "entry":self.0.entry.digest,
                "bundle":self.0.bundle.digest,
                "package":self.0.package.digest,
            })
        }
        pub(crate) fn recheck(&self) -> Result<()> {
            self.0.recheck()
        }
    }

    pub async fn run(args: &[String]) -> Result<()> {
        if matches!(args, [arg] if arg == "--help" || arg == "-h") {
            print!("{USAGE}");
            return Ok(());
        }
        if args.iter().any(|arg| arg == "--offline" || arg == "--undo") {
            return crate::legacy_migration::run(args).await;
        }
        let options = options(args)?;
        let prefix = options.prefix.as_deref().expect("validated prefix");
        let selected_config = options.config.as_deref().expect("validated config");
        let config_signature =
            signature(&crate::storage::private_metadata(selected_config, false)?);
        let configuration = DeviceConfig::read(selected_config)?;
        if signature(&crate::storage::private_metadata(selected_config, false)?) != config_signature
        {
            return Err(Error::message("Configuration changed during inspection"));
        }
        let config = selected_config.canonicalize()?;
        let layout = Layout::inspect(prefix)?;
        let startup = StartupObservation::inspect(&config, options.unit.as_deref())?;
        let service = match options.unit.as_deref() {
            Some(unit) => Some(
                systemd_identity::inspect_legacy(
                    unit,
                    prefix,
                    &config,
                    &configuration.gateway_url,
                    &layout.node.file,
                    &layout.entry.file,
                )
                .await?,
            ),
            None => None,
        };
        layout.recheck()?;
        startup.recheck()?;
        if signature(&crate::storage::private_metadata(&config, false)?) != config_signature
            || signature(&crate::storage::private_metadata(selected_config, false)?)
                != config_signature
        {
            return Err(Error::message("Configuration changed during inspection"));
        }
        println!(
            "{}",
            json!({
                "kind":"legacy-node-layout",
                "prefix":prefix,
                "version":layout.version,
                "config":config,
                "device_id":configuration.device_id,
                "config_parse":"ok",
                "sha256":{
                    "node":layout.node.digest,
                    "launcher":layout.launcher.digest,
                    "entry":layout.entry.digest,
                    "bundle":layout.bundle.digest,
                    "package":layout.package.digest,
                },
                "service":service,
                "startup":startup.value,
                "provenance":"unverified",
                "hub_ready":"unverified",
                "active_work":"unverified",
                "migration_authorized":false,
                "offline_migration":"requires_explicit_stopped_service_preflight",
            })
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        #[test]
        fn startup_binding_match_is_observational_and_changes_are_detected() {
            let directory = std::env::temp_dir().join(format!(
                "rdh-startup-inspect-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            fs::create_dir(&directory).unwrap();
            let config = directory.join("device.json");
            let sidecar = startup::sidecar_path(&config);
            crate::storage::write_private_new(
                &sidecar,
                br#"{"version":1,"binding":{"mode":"user","name":"rdh.service"},"pending":null}"#,
            )
            .unwrap();
            let matching = StartupObservation::inspect(&config, Some("rdh.service")).unwrap();
            assert_eq!(matching.value["binding_matches_user_unit"], true);
            assert!(matching.recheck().is_ok());
            let other = StartupObservation::inspect(&config, Some("other.service")).unwrap();
            assert_eq!(other.value["binding_matches_user_unit"], false);
            let no_unit = StartupObservation::inspect(&config, None).unwrap();
            assert!(no_unit.value["binding_matches_user_unit"].is_null());
            let lock = PathBuf::from(format!("{}.node-lock", sidecar.display()));
            fs::create_dir(&lock).unwrap();
            assert!(matching.recheck().is_err());
            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn wrapper_identity_changes_on_replacement_and_rejects_aliases() {
            let directory = std::env::temp_dir().join(format!(
                "rdh-wrapper-inspect-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let path = directory.join("run-agent");
            fs::write(&path, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let (_, first) = observe_wrapper(&directory, "run-agent").unwrap();
            fs::rename(&path, directory.join("old")).unwrap();
            fs::write(&path, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let (_, second) = observe_wrapper(&directory, "run-agent").unwrap();
            assert_ne!(first, second);
            let alias = directory.join("hardlink");
            fs::hard_link(&path, &alias).unwrap();
            assert!(observe_wrapper(&directory, "run-agent").is_err());
            fs::remove_file(alias).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
            assert!(observe_wrapper(&directory, "run-agent").is_err());
            fs::remove_file(&path).unwrap();
            symlink(directory.join("old"), &path).unwrap();
            assert!(observe_wrapper(&directory, "run-agent").is_err());
            fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::run;
#[cfg(target_os = "linux")]
pub(crate) use linux::{MigrationLayout, ScriptIdentity, observe_wrapper};

#[cfg(all(target_os = "macos", feature = "experimental-macos-install"))]
pub async fn run(args: &[String]) -> crate::error::Result<()> {
    crate::legacy_migration_macos::run(args)
}

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install")
)))]
pub async fn run(_: &[String]) -> crate::error::Result<()> {
    Err(crate::error::Error::message(
        "Legacy migration inspection is only implemented for Linux user installations",
    ))
}
