//! Development-only native artifact builder. It never publishes or installs.
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RUSTC_VERSION: &str = "1.93.0";
mod test_candidate;
const TARGETS: &[(&str, &str)] = &[
    ("x86_64-unknown-linux-gnu", "linux-x64-gnu"),
    ("aarch64-unknown-linux-gnu", "linux-arm64-gnu"),
    ("x86_64-unknown-linux-musl", "linux-x64-musl"),
    ("aarch64-unknown-linux-musl", "linux-arm64-musl"),
    ("x86_64-apple-darwin", "darwin-x64"),
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-pc-windows-msvc", "win32-x64"),
    ("aarch64-pc-windows-msvc", "win32-arm64"),
];

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn command(program: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    let output = Command::new(program).args(args).current_dir(cwd).output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output)
}
fn stdout(output: Output) -> Result<String> {
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
fn hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn validate_binary(path: &Path, target: &str) -> Result<()> {
    let bytes = fs::read(path)?;
    let arch = if target.starts_with("x86_64-") {
        "x86_64"
    } else {
        "aarch64"
    };
    if target.contains("linux") {
        if bytes.len() < 20 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
            return Err("Expected a 64-bit little-endian ELF binary".into());
        }
        let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
        if machine != if arch == "x86_64" { 62 } else { 183 } {
            return Err("ELF architecture does not match target".into());
        }
    } else if target.contains("apple-darwin") {
        if bytes.len() < 8 || bytes[..4] != [0xcf, 0xfa, 0xed, 0xfe] {
            return Err("Expected a 64-bit little-endian Mach-O binary".into());
        }
        let cpu = u32::from_le_bytes(bytes[4..8].try_into()?);
        if cpu
            != if arch == "x86_64" {
                0x0100_0007
            } else {
                0x0100_000c
            }
        {
            return Err("Mach-O architecture does not match target".into());
        }
    } else {
        if bytes.len() < 64 || &bytes[..2] != b"MZ" {
            return Err("Expected a PE binary".into());
        }
        let offset = u32::from_le_bytes(bytes[0x3c..0x40].try_into()?) as usize;
        if offset.checked_add(6).is_none_or(|end| end > bytes.len())
            || &bytes[offset..offset + 4] != b"PE\0\0"
        {
            return Err("Invalid PE header".into());
        }
        let machine = u16::from_le_bytes(bytes[offset + 4..offset + 6].try_into()?);
        if machine != if arch == "x86_64" { 0x8664 } else { 0xaa64 } {
            return Err("PE architecture does not match target".into());
        }
    }
    Ok(())
}

fn copy_licenses(
    root: &Path,
    target: &str,
    metadata: &Value,
    destination: &Path,
) -> Result<Vec<Value>> {
    let tree = stdout(command(
        "cargo",
        &[
            "tree",
            "-p",
            "rdh-agent",
            "--locked",
            "--target",
            target,
            "-e",
            "normal",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ],
        root,
    )?)?;
    let active: BTreeSet<_> = tree
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let name = fields.next()?;
            let version = fields.next()?.strip_prefix('v')?;
            Some((name.to_owned(), version.to_owned()))
        })
        .collect();
    let mut found = BTreeSet::new();
    let mut result = Vec::new();
    for package in metadata["packages"]
        .as_array()
        .ok_or("Cargo metadata has no packages")?
    {
        if package["source"].is_null() {
            continue;
        }
        let name = package["name"].as_str().ok_or("Missing dependency name")?;
        let version = package["version"]
            .as_str()
            .ok_or("Missing dependency version")?;
        if !active.contains(&(name.to_owned(), version.to_owned())) {
            continue;
        }
        let license = package["license"]
            .as_str()
            .ok_or("Dependency license expression is missing")?;
        let source = Path::new(
            package["manifest_path"]
                .as_str()
                .ok_or("Dependency manifest is missing")?,
        )
        .parent()
        .ok_or("Dependency source directory is missing")?;
        let directory = destination.join(format!("{name}-{version}"));
        let mut files = Vec::new();
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            let lower = name.to_string_lossy().to_ascii_lowercase();
            let kind = entry.file_type()?;
            if kind.is_file()
                && ["license", "licence", "copying", "notice"]
                    .iter()
                    .any(|prefix| lower.starts_with(prefix))
            {
                files.push((name, entry.path()));
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        if files.is_empty() {
            return Err(format!("No packaged license text found for {name} {version}").into());
        }
        fs::create_dir_all(&directory)?;
        let mut names = Vec::new();
        for (filename, path) in files {
            fs::copy(path, directory.join(&filename))?;
            names.push(filename.to_string_lossy().into_owned());
        }
        found.insert((name.to_owned(), version.to_owned()));
        result.push(json!({"name":name,"version":version,"license":license,"files":names}));
    }
    // The workspace binary itself is the only non-registry package in this tree.
    let missing: Vec<_> = active
        .iter()
        .filter(|(name, _)| name != "rdh-agent")
        .filter(|item| !found.contains(*item))
        .collect();
    if !missing.is_empty() {
        return Err(format!("Missing dependency license metadata: {missing:?}").into());
    }
    result.sort_by(|a, b| {
        a["name"]
            .as_str()
            .cmp(&b["name"].as_str())
            .then_with(|| a["version"].as_str().cmp(&b["version"].as_str()))
    });
    Ok(result)
}

fn build(target: &str, output_dir: &Path) -> Result<()> {
    let platform = TARGETS
        .iter()
        .find(|(candidate, _)| *candidate == target)
        .map(|(_, platform)| *platform)
        .ok_or("Unsupported target; use one of the eight documented release targets")?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("Workspace root missing")?;
    let rustc = stdout(command("rustc", &["--version"], root)?)?;
    if rustc.split_whitespace().nth(1) != Some(RUSTC_VERSION) {
        return Err(
            format!("Development builds require rustc {RUSTC_VERSION}; found {rustc}").into(),
        );
    }
    let detail = stdout(command("rustc", &["-vV"], root)?)?;
    let host = detail
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or("rustc host is missing")?;
    let metadata: Value = serde_json::from_slice(
        &command(
            "cargo",
            &["metadata", "--locked", "--format-version", "1"],
            root,
        )?
        .stdout,
    )?;
    let target_dir = PathBuf::from(
        metadata["target_directory"]
            .as_str()
            .ok_or("Cargo target directory is missing")?,
    );
    let status = Command::new("cargo")
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            "rdh-agent",
            "--target",
            target,
        ])
        .current_dir(root)
        .status()?;
    if !status.success() {
        return Err("Agent release build failed".into());
    }
    let binary_name = if target.contains("windows") {
        "rdh.exe"
    } else {
        "rdh"
    };
    let binary = target_dir.join(target).join("release").join(binary_name);
    validate_binary(&binary, target)?;
    let mut runtime_check = false;
    if target == host {
        let output = Command::new(&binary)
            .arg("--version")
            .env_clear()
            .env("PATH", "")
            .output()?;
        if !output.status.success()
            || String::from_utf8(output.stdout)?.trim() != env!("CARGO_PKG_VERSION")
        {
            return Err("Native binary version self-check failed".into());
        }
        runtime_check = true;
    }
    fs::create_dir_all(output_dir)?;
    let output_dir = output_dir.canonicalize()?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let stage =
        Staging(output_dir.join(format!(".rdh-native-stage-{}-{nonce}", std::process::id())));
    fs::create_dir(&stage.0)?;
    let basename = format!("rdh-{}-{platform}", env!("CARGO_PKG_VERSION"));
    let tree = stage.0.join(&basename);
    fs::create_dir(&tree)?;
    fs::copy(&binary, tree.join(binary_name))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !target.contains("windows") {
            fs::set_permissions(tree.join(binary_name), fs::Permissions::from_mode(0o755))?;
        }
    }
    let dependencies = copy_licenses(root, target, &metadata, &tree.join("licenses"))?;
    let source_commit = stdout(command("git", &["rev-parse", "HEAD"], root)?)?;
    let source_dirty = !command(
        "git",
        &["status", "--porcelain", "--untracked-files=all"],
        root,
    )?
    .stdout
    .is_empty();
    let project_license = ["LICENSE", "LICENSE.md", "LICENSE.txt"]
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file());
    if let Some(license) = &project_license {
        fs::copy(license, tree.join("PROJECT_LICENSE"))?;
    }
    let manifest = json!({"schema":1,"kind":"native-agent-development", "version":env!("CARGO_PKG_VERSION"),
        "target":target,"platform":platform,"rustc":rustc,"source_commit":source_commit,"source_dirty":source_dirty,
        "runtime_self_check":runtime_check,"release_ready":false,
        "project_license_included":project_license.is_some(),
        "binary":{"path":binary_name,"bytes":fs::metadata(&binary)?.len(),"sha256":hash(&binary)?},
        "dependencies":dependencies});
    fs::write(
        tree.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    let archive = output_dir.join(format!("{basename}.{extension}"));
    let checksum = output_dir.join(format!("{basename}.{extension}.sha256"));
    let raw_name = if target.contains("windows") {
        format!("{basename}.exe")
    } else {
        basename.clone()
    };
    let raw = output_dir.join(&raw_name);
    let raw_checksum = output_dir.join(format!("{raw_name}.sha256"));
    let channel = output_dir.join(format!("{basename}.channel.json"));
    if [&archive, &checksum, &raw, &raw_checksum, &channel]
        .iter()
        .any(|path| fs::symlink_metadata(path).is_ok())
    {
        return Err("Artifact or checksum already exists; choose another output directory".into());
    }
    let temporary_archive = stage.0.join(format!("archive.{extension}"));
    let status = if target.contains("windows") {
        Command::new("zip")
            .args([
                "-qr",
                temporary_archive
                    .to_str()
                    .ok_or("Non-Unicode output path")?,
                &basename,
            ])
            .current_dir(&stage.0)
            .status()?
    } else {
        Command::new("tar")
            .args([
                "-czf",
                temporary_archive
                    .to_str()
                    .ok_or("Non-Unicode output path")?,
                "-C",
                stage.0.to_str().ok_or("Non-Unicode staging path")?,
                &basename,
            ])
            .status()?
    };
    if !status.success() {
        return Err("Archive creation failed".into());
    }
    let archive_hash = hash(&temporary_archive)?;
    let raw_hash = hash(&binary)?;
    let index = json!({"schema":2,"channel":"canary","kind":"native-development",
        "version":env!("CARGO_PKG_VERSION"),"rollout":{"percent":100},
        "platforms":{(platform):{"format":"native-binary","url":raw_name,
            "sha256":raw_hash,"bytes":fs::metadata(&binary)?.len()}}});
    let outputs = [
        (
            &checksum,
            format!(
                "{archive_hash}  {}\n",
                archive
                    .file_name()
                    .ok_or("Archive name missing")?
                    .to_string_lossy()
            ),
        ),
        (&raw_checksum, format!("{raw_hash}  {raw_name}\n")),
        (
            &channel,
            format!("{}\n", serde_json::to_string_pretty(&index)?),
        ),
    ];
    let mut created: Vec<PathBuf> = Vec::new();
    let commit = (|| -> Result<()> {
        for (path, text) in outputs {
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            created.push(path.to_path_buf());
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        fs::hard_link(&temporary_archive, &archive)?;
        created.push(archive.clone());
        fs::hard_link(tree.join(binary_name), &raw)?;
        created.push(raw.clone());
        #[cfg(unix)]
        File::open(&output_dir)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = commit {
        for path in created.into_iter().rev() {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    println!(
        "Development archive: {}\nArchive SHA-256: {}\nNative binary: {}\nBinary SHA-256: {}\nDevelopment index: {}\nNative self-check: {}\nRelease ready: false",
        archive.display(),
        archive_hash,
        raw.display(),
        raw_hash,
        channel.display(),
        runtime_check
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.len() == 5 && args[1] == "--target" && args[3] == "--out" {
        match args[0].as_str() {
            "native-development" => build(&args[2], Path::new(&args[4])),
            "native-test-candidate" => test_candidate::build(&args[2], Path::new(&args[4])),
            _ => Err("Unknown native build command".into()),
        }
    } else {
        Err(
            "usage: cargo run -p xtask -- native-development|native-test-candidate --target TARGET --out DIRECTORY"
                .into(),
        )
    };
    if let Err(error) = result {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}
