//! Installation ownership and read-only command diagnostics.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::Result;

fn npm_entry() -> Option<PathBuf> {
    std::env::var_os("RDH_NPM_ENTRY")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn npm_package_file(path: &Path) -> bool {
    let Some(package) = path.parent() else {
        return false;
    };
    let Some(scope) = package.parent() else {
        return false;
    };
    package
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == "remote-device-agent" || name.starts_with("remote-device-agent-")
        })
        && scope.file_name().is_some_and(|name| name == "@easelian")
        && scope
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "node_modules")
        && package.join("package.json").is_file()
}

pub(crate) fn npm_managed() -> Result<bool> {
    Ok(npm_entry().is_some() || npm_package_file(&std::env::current_exe()?.canonicalize()?))
}

fn invocation(executable: &Path) -> Option<PathBuf> {
    let invoked = PathBuf::from(std::env::args_os().next()?);
    let cwd = std::env::current_dir().ok()?;
    let matches = |path: &Path| path.canonicalize().is_ok_and(|actual| actual == executable);
    if invoked.is_absolute() || invoked.components().count() > 1 {
        let path = cwd.join(invoked);
        return matches(&path).then_some(path);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| cwd.join(dir).join(&invoked))
        .find(|path| matches(path))
}

#[cfg(windows)]
const COMMAND_NAMES: &[&str] = &["rdh.exe", "rdh.cmd", "rdh.bat", "rdh.com", "rdh.ps1"];
#[cfg(not(windows))]
const COMMAND_NAMES: &[&str] = &["rdh"];

fn extra_directories() -> Vec<PathBuf> {
    let mut dirs: Vec<_> = std::env::var_os("RDH_INSTALL_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .into_iter()
        .collect();
    #[cfg(windows)]
    {
        dirs.extend(
            std::env::var_os("LOCALAPPDATA")
                .map(|base| PathBuf::from(base).join("RemoteDeviceAgent")),
        );
        dirs.extend(std::env::var_os("APPDATA").map(|base| PathBuf::from(base).join("npm")));
    }
    #[cfg(not(windows))]
    dirs.extend(
        crate::config::home_directory()
            .ok()
            .map(|home| home.join(".local/bin")),
    );
    dirs
}

#[cfg(windows)]
fn npm_shim_target(entry: &Path) -> Option<PathBuf> {
    use std::io::Read;

    let extension = entry.extension()?.to_str()?;
    if !["cmd", "ps1"]
        .iter()
        .any(|ext| extension.eq_ignore_ascii_case(ext))
    {
        return None;
    }
    let directory = entry.parent()?;
    let modules = if directory.file_name().is_some_and(|name| name == ".bin") {
        directory.parent()?.to_path_buf()
    } else {
        directory.join("node_modules")
    };
    let launcher = modules.join("@easelian/remote-device-agent/rdh.cjs");
    if !launcher.is_file() {
        return None;
    }
    let mut prefix = String::new();
    std::fs::File::open(entry)
        .ok()?
        .take(8192)
        .read_to_string(&mut prefix)
        .ok()?;
    prefix
        .replace('\\', "/")
        .to_ascii_lowercase()
        .contains("@easelian/remote-device-agent/rdh.cjs")
        .then_some(launcher)
}

fn command_target(entry: &Path) -> Option<PathBuf> {
    let metadata = entry.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    #[cfg(windows)]
    if let Some(launcher) = npm_shim_target(entry) {
        return launcher.canonicalize().ok();
    }
    entry.canonicalize().ok()
}

fn installation_key(target: &Path) -> PathBuf {
    if npm_package_file(target)
        && let Some(launcher) = target
            .ancestors()
            .map(|ancestor| ancestor.join("rdh.cjs"))
            .find(|launcher| npm_package_file(launcher) && launcher.is_file())
    {
        return launcher;
    }
    target.to_path_buf()
}

fn print_other_installations(entry: &Path, executable: &Path) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut seen = HashSet::from([installation_key(executable)]);
    if let Some(current) = command_target(entry) {
        seen.insert(installation_key(&current));
    }
    let path = std::env::var_os("PATH");
    let directories = path.as_deref().into_iter().flat_map(std::env::split_paths);
    let mut found = false;
    for directory in directories.chain(extra_directories()) {
        for name in COMMAND_NAMES {
            let candidate = cwd.join(&directory).join(name);
            if let Some(target) = command_target(&candidate)
                && seen.insert(installation_key(&target))
            {
                if !found {
                    println!("Other rdh installations:");
                    found = true;
                }
                let source = if npm_package_file(&target) {
                    "npm"
                } else {
                    "native or custom"
                };
                println!("  {} ({source})", candidate.display());
            }
        }
    }
    if !found {
        println!("Other rdh installations: none found");
    }
    Ok(())
}

pub fn diagnose() -> Result<()> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let npm_entry = npm_entry();
    let managed_by_npm = npm_entry.is_some() || npm_package_file(&executable);
    let entry = npm_entry
        .or_else(|| invocation(&executable))
        .unwrap_or_else(|| executable.clone());
    println!("Command entry: {}", entry.display());
    println!("Native executable: {}", executable.display());
    println!(
        "Installation source: {}",
        if managed_by_npm { "npm" } else { "native" }
    );

    print_other_installations(&entry, &executable)?;

    let config = crate::binding::prepare_options(false)?.config;
    match crate::startup::diagnostic_binding(&config) {
        Ok(Some(binding)) => {
            println!("Startup registration: {}", binding.name);
            match crate::startup::diagnostic_executable(&config, &binding) {
                Ok(path) => println!("Startup executable: {}", path.display()),
                Err(error) => println!("Startup executable: unavailable ({error})"),
            }
        }
        Ok(None) => println!("Startup executable: no registration recorded for this profile"),
        Err(error) => println!("Startup executable: unavailable ({error})"),
    }
    Ok(())
}
