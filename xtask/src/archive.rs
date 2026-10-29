//! Round-trip archives produced from this build's staging tree.
//! This is not a general extractor for downloaded or untrusted archives.
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use super::{Result, hash};

#[cfg(windows)]
fn tar() -> Result<PathBuf> {
    let root = std::env::var_os("SystemRoot").ok_or("SystemRoot is missing")?;
    let executable = PathBuf::from(root).join("System32").join("tar.exe");
    if !executable.is_absolute() || !executable.is_file() {
        return Err("Windows system tar.exe is unavailable".into());
    }
    Ok(executable)
}

#[cfg(not(windows))]
fn tar() -> Result<PathBuf> {
    Ok(PathBuf::from("tar"))
}

fn execute(command: &mut Command) -> Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "Archive operation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

pub(super) fn create(
    archive: &Path,
    stage: &Path,
    basename: &str,
    windows_target: bool,
) -> Result<()> {
    if windows_target {
        #[cfg(windows)]
        execute(
            Command::new(tar()?)
                .args(["--format=zip", "-cf"])
                .arg(archive)
                .arg(basename)
                .current_dir(stage),
        )?;
        #[cfg(not(windows))]
        execute(
            Command::new("zip")
                .arg("-qr")
                .arg(archive)
                .arg(basename)
                .current_dir(stage),
        )?;
        let mut header = [0u8; 4];
        fs::File::open(archive)?.read_exact(&mut header)?;
        if &header != b"PK\x03\x04" {
            return Err("The Windows artifact is not a ZIP archive".into());
        }
    } else {
        execute(
            Command::new(tar()?)
                .arg("-czf")
                .arg(archive)
                .arg("-C")
                .arg(stage)
                .arg(basename),
        )?;
    }
    Ok(())
}

pub(super) fn inventory(tree: &Path) -> Result<BTreeMap<String, Value>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, Value>) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path.strip_prefix(root)?;
            if relative.components().count() > 8 || files.len() >= 4096 {
                return Err("Native archive contains too many entries".into());
            }
            let name = relative
                .to_str()
                .ok_or("Non-Unicode native archive entry")?
                .replace('\\', "/");
            let metadata = fs::symlink_metadata(&path)?;
            let value = if metadata.file_type().is_dir() {
                json!({"kind":"directory"})
            } else if metadata.file_type().is_file() {
                #[cfg(unix)]
                let executable = {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o111
                };
                #[cfg(not(unix))]
                let executable = 0;
                json!({"kind":"file", "bytes":metadata.len(),
                    "sha256":hash(&path)?, "executable":executable})
            } else {
                return Err("Native archives cannot contain links or special files".into());
            };
            if files.insert(name, value).is_some() {
                return Err("Native archive contains a duplicate entry".into());
            }
            if metadata.is_dir() {
                visit(root, &path, files)?;
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(tree, tree, &mut files)?;
    Ok(files)
}

pub(super) fn restore_and_compare(
    archive: &Path,
    source: &Path,
    destination: &Path,
    basename: &str,
    windows_target: bool,
) -> Result<PathBuf> {
    fs::create_dir(destination)?;
    #[cfg(not(windows))]
    if windows_target {
        execute(
            Command::new("unzip")
                .arg("-q")
                .arg(archive)
                .arg("-d")
                .arg(destination),
        )?;
    } else {
        execute(
            Command::new(tar()?)
                .arg("-xf")
                .arg(archive)
                .arg("-C")
                .arg(destination),
        )?;
    }
    #[cfg(windows)]
    {
        let _ = windows_target;
        execute(
            Command::new(tar()?)
                .arg("-xf")
                .arg(archive)
                .arg("-C")
                .arg(destination),
        )?;
    }
    let roots = fs::read_dir(destination)?.collect::<std::io::Result<Vec<_>>>()?;
    if roots.len() != 1 || roots[0].file_name() != basename || !roots[0].file_type()?.is_dir() {
        return Err("Archive extraction has an unexpected top-level entry".into());
    }
    let restored = destination.join(basename);
    let expected = inventory(source)?;
    if inventory(&restored)? != expected {
        return Err("Extracted archive differs from the staged files or permissions".into());
    }
    println!("Archive round-trip verified: {} entries", expected.len());
    Ok(restored)
}
