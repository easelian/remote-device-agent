//! Compile a distinct version for upgrade tests; never patch executable bytes.
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use serde_json::json;
use sha2::{Digest, Sha256};

use super::{RUSTC_VERSION, Result, Staging, TARGETS, command, hash, stdout, validate_binary};

fn copy_source(source: &Path, destination: &Path, digest: &mut Sha256) -> Result<()> {
    fs::create_dir(destination)?;
    let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name.to_str().ok_or("Non-Unicode source filename")?;
        let kind = entry.file_type()?;
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        if kind.is_dir() {
            digest.update(b"directory");
            copy_source(&entry.path(), &destination.join(name), digest)?;
        } else if kind.is_file() {
            let bytes = fs::read(entry.path())?;
            digest.update(b"file");
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(&bytes);
            fs::write(destination.join(name), bytes)?;
        } else {
            return Err("Upgrade fixture source cannot contain links or special files".into());
        }
    }
    digest.update(b"end-directory");
    Ok(())
}

fn replace_once(text: &str, before: &str, after: &str) -> Result<String> {
    if text.matches(before).count() != 1 {
        return Err("Upgrade fixture manifest does not have one expected version entry".into());
    }
    Ok(text.replacen(before, after, 1))
}

pub(super) fn build(target: &str, output: &Path) -> Result<()> {
    if !target.contains("-linux-") || !TARGETS.iter().any(|(value, _)| *value == target) {
        return Err(
            "Native upgrade fixtures currently require one of the four Linux targets".into(),
        );
    }
    if !output.is_absolute() {
        return Err("Upgrade fixture output directory must be absolute and new".into());
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Workspace root missing")?;
    let rustc = stdout(command("rustc", &["--version"], root)?)?;
    if rustc.split_whitespace().nth(1) != Some(RUSTC_VERSION) {
        return Err(format!("Upgrade fixture requires Rust {RUSTC_VERSION}; found {rustc}").into());
    }
    let source_version = env!("CARGO_PKG_VERSION");
    let core = source_version.split('-').next().unwrap();
    let (major_minor, patch) = core.rsplit_once('.').ok_or("Invalid workspace version")?;
    let patch = patch
        .parse::<u64>()?
        .checked_add(1)
        .ok_or("Workspace patch version overflow")?;
    let version = format!("{major_minor}.{patch}-native-test.1");
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::DirBuilderExt;
        let mut directory = fs::DirBuilder::new();
        directory.mode(0o700);
        directory
    };
    #[cfg(not(unix))]
    let directory = fs::DirBuilder::new();
    directory.create(output)?;
    let output = output.canonicalize()?;
    let source = Staging(output.join("source"));
    fs::create_dir(&source.0)?;
    let target_dir = Staging(output.join("target"));
    fs::create_dir(&target_dir.0)?;
    let mut digest = Sha256::new();
    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/rdh-agent/Cargo.toml",
        "xtask/Cargo.toml",
    ] {
        let path = root.join(relative);
        if !fs::symlink_metadata(&path)?.file_type().is_file() {
            return Err("Upgrade fixture manifests must be regular files".into());
        }
        let bytes = fs::read(path)?;
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(&bytes);
        let destination = source.0.join(relative);
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::write(destination, bytes)?;
    }
    for relative in ["crates/rdh-agent/src", "xtask/src"] {
        digest.update((relative.len() as u64).to_le_bytes());
        digest.update(relative.as_bytes());
        copy_source(&root.join(relative), &source.0.join(relative), &mut digest)?;
    }
    let source_sha256 = format!("{:x}", digest.finalize());
    let manifest = source.0.join("Cargo.toml");
    fs::write(
        &manifest,
        replace_once(
            &fs::read_to_string(&manifest)?,
            &format!("version = \"{source_version}\""),
            &format!("version = \"{version}\""),
        )?,
    )?;
    let lock = source.0.join("Cargo.lock");
    let mut contents = fs::read_to_string(&lock)?;
    for package in ["rdh-agent", "xtask"] {
        contents = replace_once(
            &contents,
            &format!("name = \"{package}\"\nversion = \"{source_version}\""),
            &format!("name = \"{package}\"\nversion = \"{version}\""),
        )?;
    }
    fs::write(lock, contents)?;
    // Keep this build separate from the invoking cargo process and its locks.
    // Dependencies and their checksums remain fixed by the copied lockfile.
    let status = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--release",
            "--all-features",
            "-p",
            "rdh-agent",
            "--bin",
            "rdh",
            "--target",
            target,
        ])
        .current_dir(&source.0)
        .env("CARGO_TARGET_DIR", &target_dir.0)
        .status()?;
    if !status.success() {
        return Err("Native upgrade fixture build failed".into());
    }
    let built = target_dir.0.join(target).join("release/rdh");
    validate_binary(&built, target)?;
    let binary = output.join("rdh");
    fs::copy(built, &binary)?;
    let mut verified_version = false;
    if std::env::consts::OS == "linux" && target.starts_with(std::env::consts::ARCH) {
        let result = Command::new(&binary)
            .arg("--version")
            .env_clear()
            .env("PATH", "")
            .output()?;
        if !result.status.success() || String::from_utf8(result.stdout)?.trim() != version {
            return Err("Compiled upgrade fixture reports an unexpected version".into());
        }
        verified_version = true;
    }
    File::open(&binary)?.sync_all()?;
    let manifest = json!({"schema":1,"kind":"native-test-candidate","source_version":source_version,
        "version":version,"target":target,"source_sha256":source_sha256,"rustc":rustc,
        "binary":{"sha256":hash(&binary)?,"bytes":fs::metadata(&binary)?.len()},
        "version_executed":verified_version,"release_ready":false});
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("candidate.json"))?;
    file.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!(
        "Native upgrade fixture: {}\nVersion: {version}\nSource SHA-256: {source_sha256}",
        output.display()
    );
    Ok(())
}
