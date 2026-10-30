//! Prepared Linux bubblewrap mounts and shell resource limits.
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

use crate::config::DeviceConfig;
#[cfg(target_os = "linux")]
use crate::config::{config_base, home_directory};
use crate::error::{Error, Result};
use crate::policy::Policy;
#[cfg(target_os = "linux")]
use crate::policy::sidecar_path;

#[derive(Clone)]
struct Bwrap {
    executable: PathBuf,
    network: bool,
    hide_home: bool,
    home: PathBuf,
    writable: Vec<PathBuf>,
    readonly: Vec<PathBuf>,
    expose: Vec<String>,
    hidden_paths: Vec<PathBuf>,
    hidden_files: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct Sandbox {
    shell: String,
    limits: std::collections::BTreeMap<String, u64>,
    bwrap: Option<Bwrap>,
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(Error::message("Sandbox paths must be absolute"));
    }
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| Error::message("Invalid sandbox path"))?;
            let name = path
                .file_name()
                .ok_or_else(|| Error::message("Invalid sandbox path"))?;
            Ok(canonical_path(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}
fn sorted_unique(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let unique: BTreeSet<_> = paths.into_iter().collect();
    let mut result: Vec<_> = unique.into_iter().collect();
    result.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    result
}
fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::message("Sandbox path is not valid Unicode"))
}

#[cfg(target_os = "linux")]
fn find_bwrap() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let paths =
        std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
    for directory in std::env::split_paths(&paths) {
        let candidate = directory.join("bwrap");
        if let Ok(metadata) = candidate.metadata()
            && metadata.is_file()
            && metadata.permissions().mode() & 0o111 != 0
        {
            return Ok(candidate.canonicalize()?);
        }
    }
    Err(Error::message(
        "bubblewrap is unavailable; sandbox.mode bwrap requires isolation to start",
    ))
}

#[cfg(target_os = "linux")]
fn probe(executable: &Path) -> Result<()> {
    use std::process::{Command, Stdio};
    let true_path = if Path::new("/usr/bin/true").is_file() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let mut child = Command::new(executable)
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--unshare-pid",
            "--die-with-parent",
            "--tmpfs",
            "/tmp",
            true_path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            Error::message("bubblewrap is unavailable or cannot create a user namespace")
        })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                return Err(Error::message(
                    "bubblewrap is unavailable or cannot create a user namespace",
                ));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::message("bubblewrap isolation probe timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Sandbox {
    pub fn prepare(
        config: &DeviceConfig,
        policy: &Policy,
        config_path: Option<&Path>,
    ) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        let _ = config_path;
        let configured = policy.sandbox.as_ref();
        let limits = configured.map_or_else(Default::default, |sandbox| sandbox.limits.clone());
        let bwrap = if configured.is_some_and(|sandbox| sandbox.mode == "bwrap") {
            #[cfg(not(target_os = "linux"))]
            {
                return Err(Error::message(
                    "bubblewrap sandbox is supported only on Linux",
                ));
            }
            #[cfg(target_os = "linux")]
            {
                let sandbox = configured.expect("checked above");
                let config_path = config_path.ok_or_else(|| {
                    Error::message("A config path is required to protect sandbox credentials")
                })?;
                let executable = find_bwrap()?;
                probe(&executable)?;
                let home = canonical_path(&home_directory()?)?;
                let config_dir = config_base()?;
                let state_dir = std::env::var_os("XDG_STATE_HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or(home.join(".local/state"));
                let hidden_paths = [
                    config_dir.join("remote-device-hub"),
                    config_dir.join("remote-device-host"),
                    state_dir.join("remote-device-hub"),
                ]
                .iter()
                .map(|path| canonical_path(path))
                .collect::<Result<Vec<_>>>()?;
                let hidden_files = [config_path.to_path_buf(), sidecar_path(config_path)]
                    .iter()
                    .map(|path| canonical_path(path))
                    .collect::<Result<Vec<_>>>()?;
                let mut writable = Vec::new();
                let mut readonly = Vec::new();
                let mut expose = Vec::new();
                for (name, root) in &config.roots {
                    let root = canonical_path(Path::new(root))?;
                    if config.allow_write && !policy.readonly_roots.contains(name) {
                        writable.push(root);
                    } else {
                        readonly.push(root);
                    }
                }
                for path in &sandbox.expose {
                    if Path::new(path).exists() {
                        expose.push(path.clone());
                        readonly.push(canonical_path(Path::new(path))?);
                    }
                }
                Some(Bwrap {
                    executable,
                    network: sandbox.network.expect("validated by policy"),
                    hide_home: sandbox.hide_home.expect("validated by policy"),
                    home,
                    writable: sorted_unique(writable),
                    readonly: sorted_unique(readonly),
                    expose,
                    hidden_paths: sorted_unique(hidden_paths),
                    hidden_files: sorted_unique(hidden_files),
                })
            }
        } else {
            None
        };
        #[cfg(not(unix))]
        if !limits.is_empty() {
            return Err(Error::message(
                "POSIX resource limits are unsupported on this platform",
            ));
        }
        Ok(Self {
            shell: config.shell.clone(),
            limits,
            bwrap,
        })
    }

    pub fn is_isolated(&self) -> bool {
        self.bwrap.is_some()
    }
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({"mode":if self.bwrap.is_some() {"bwrap"} else {"none"},
            "network":self.bwrap.as_ref().is_none_or(|bwrap| bwrap.network),
            "hide_home":self.bwrap.as_ref().is_some_and(|bwrap| bwrap.hide_home),"limits":self.limits})
    }
    pub fn identity_paths(&self) -> (Vec<String>, Vec<String>, Vec<String>) {
        let Some(bwrap) = &self.bwrap else {
            return (vec![], vec![], vec![]);
        };
        let strings = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        };
        (
            bwrap.expose.clone(),
            strings(&bwrap.hidden_paths),
            strings(&bwrap.hidden_files),
        )
    }
    pub fn command(&self, workdir: &Path, command: &str) -> Result<(PathBuf, Vec<OsString>)> {
        let mut script = String::new();
        for (name, flag) in [
            ("cpu_seconds", 't'),
            ("memory_kb", 'v'),
            ("open_files", 'n'),
            ("file_size_blocks", 'f'),
        ] {
            if let Some(value) = self.limits.get(name) {
                script.push_str(&format!("ulimit -{flag} {value} || exit 126; "));
            }
        }
        script.push_str("exec 2>&1; ");
        script.push_str(command);
        let Some(bwrap) = &self.bwrap else {
            return Ok((PathBuf::from(&self.shell), vec!["-c".into(), script.into()]));
        };
        let workdir = canonical_path(workdir)?;
        let mut args: Vec<OsString> = [
            "--die-with-parent",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        if !bwrap.network {
            args.push("--unshare-net".into());
        }
        for value in [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ] {
            args.push(value.into());
        }
        if bwrap.hide_home {
            args.extend([
                OsString::from("--tmpfs"),
                bwrap.home.clone().into_os_string(),
            ]);
        }
        let bind_trees = |args: &mut Vec<OsString>, below: Option<&Path>| {
            for (writable, paths) in [(true, &bwrap.writable), (false, &bwrap.readonly)] {
                for root in paths {
                    if below.is_some_and(|parent| !root.starts_with(parent)) {
                        continue;
                    }
                    let flag = if writable
                        && !bwrap.readonly.iter().any(|parent| root.starts_with(parent))
                    {
                        "--bind"
                    } else {
                        "--ro-bind"
                    };
                    args.extend([
                        OsString::from(flag),
                        root.clone().into_os_string(),
                        root.clone().into_os_string(),
                    ]);
                }
            }
        };
        bind_trees(&mut args, None);
        let mut private = vec![PathBuf::from("/tmp")];
        if bwrap.hide_home {
            private.push(bwrap.home.clone());
        }
        for path in sorted_unique(private) {
            args.extend([OsString::from("--tmpfs"), path.clone().into_os_string()]);
            bind_trees(&mut args, Some(&path));
        }
        for value in ["--dev", "/dev", "--proc", "/proc"] {
            args.push(value.into());
        }
        let mut masks: Vec<PathBuf> = Vec::new();
        for path in &bwrap.hidden_paths {
            if !masks.iter().any(|parent| path.starts_with(parent)) {
                masks.push(path.clone());
            }
        }
        for path in &masks {
            args.extend([OsString::from("--tmpfs"), path.clone().into_os_string()]);
        }
        for path in &bwrap.hidden_files {
            if !masks.iter().any(|parent| path.starts_with(parent)) {
                args.extend([
                    OsString::from("--ro-bind"),
                    OsString::from("/dev/null"),
                    path.clone().into_os_string(),
                ]);
            }
        }
        path_text(&workdir)?;
        args.extend([
            OsString::from("--chdir"),
            workdir.into_os_string(),
            OsString::from("--"),
            OsString::from(&self.shell),
            OsString::from("-c"),
            OsString::from(script),
        ]);
        Ok((bwrap.executable.clone(), args))
    }
}
