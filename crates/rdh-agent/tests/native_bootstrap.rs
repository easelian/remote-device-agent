#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-install"),
    all(windows, feature = "experimental-windows-install")
))]

#[path = "support/initial_binary.rs"]
mod initial_binary;
#[cfg(windows)]
#[allow(dead_code)]
#[path = "support/windows.rs"]
mod windows;

use rdh_agent::{config::sha256_hex, random};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rdh bootstrap ' 雪 {}",
            random::hex_token::<8>().unwrap()
        ));
        #[cfg(unix)]
        rdh_agent::storage::create_private_dir(&path).unwrap();
        #[cfg(windows)]
        windows::private_directory(&path);
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Retaining failed bootstrap fixture: {}", self.0.display());
        } else {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

fn artifact_name(version: &str) -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => panic!("Unsupported fixture architecture"),
    };
    #[cfg(target_os = "linux")]
    let platform = format!(
        "linux-{arch}-{}",
        if cfg!(target_env = "musl") {
            "musl"
        } else {
            "gnu"
        }
    );
    #[cfg(target_os = "macos")]
    let platform = format!("darwin-{arch}");
    #[cfg(windows)]
    let platform = format!("win32-{arch}.exe");
    format!("rdh-{version}-{platform}")
}

fn artifact(directory: &Path, version: &str, bytes: &[u8]) -> PathBuf {
    let name = artifact_name(version);
    let path = directory.join(&name);
    fs::write(&path, bytes).unwrap();
    fs::write(
        directory.join(format!("{name}.sha256")),
        format!("{}  {name}\n", sha256_hex(bytes)),
    )
    .unwrap();
    path
}

fn bootstrap(directory: &Path, version: &str, prefix: &Path, offline: bool) -> Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let binary = directory.join(artifact_name(version));
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.arg(repository.join("packaging/install.sh"));
        #[cfg(target_os = "linux")]
        command.args([
            "--libc",
            if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            },
        ]);
        if offline {
            command.arg("--offline").arg(&binary);
        } else {
            command
                .args(["--version", version])
                .arg("--base")
                .arg(directory);
        }
        command.args(["--", "--prefix"]).arg(prefix);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let system = std::env::var_os("SystemRoot").expect("Windows system directory");
        let mut command =
            Command::new(Path::new(&system).join("System32/WindowsPowerShell/v1.0/powershell.exe"));
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ]);
        command.arg(if offline {
            "& $env:RDH_TEST_INSTALLER -Offline $env:RDH_TEST_BINARY -NativeArguments @('--prefix', $env:RDH_TEST_PREFIX)"
        } else {
            "& $env:RDH_TEST_INSTALLER -Version $env:RDH_TEST_VERSION -Base $env:RDH_TEST_BASE -NativeArguments @('--prefix', $env:RDH_TEST_PREFIX)"
        });
        command
            .env(
                "RDH_TEST_INSTALLER",
                repository.join("packaging/install.ps1"),
            )
            .env("RDH_TEST_BINARY", binary)
            .env("RDH_TEST_PREFIX", prefix)
            .env("RDH_TEST_VERSION", version)
            .env("RDH_TEST_BASE", directory);
        command
    };
    command.current_dir(directory).output().unwrap()
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn failure(output: Output, message: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success() && stderr.to_lowercase().contains(message),
        "Expected {message:?}, got {}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
}

fn selected_binary(prefix: &Path, version: &str) -> PathBuf {
    prefix
        .join("versions")
        .join(version)
        .join(if cfg!(windows) { "rdh.exe" } else { "rdh" })
}

#[test]
fn bootstrap_verifies_native_artifact_and_preserves_existing_installation() {
    let fixture = Fixture::new();
    let version = env!("CARGO_PKG_VERSION");
    let bytes = fs::read(initial_binary::path()).unwrap();
    let binary = artifact(&fixture.0, version, &bytes);
    let checksum = fixture.0.join(format!(
        "{}.sha256",
        binary.file_name().unwrap().to_str().unwrap()
    ));
    let checksum_bytes = fs::read(&checksum).unwrap();
    let refused = fixture.0.join("refused install 雪");
    fs::write(
        &checksum,
        format!("{}  {}\n", "0".repeat(64), artifact_name(version)),
    )
    .unwrap();
    failure(
        bootstrap(&fixture.0, version, &refused, true),
        "checksum mismatch",
    );
    assert!(!refused.exists());
    fs::write(&checksum, checksum_bytes).unwrap();

    let wrong_version = "9999.0.0-bootstrap-fixture";
    artifact(&fixture.0, wrong_version, &bytes);
    failure(
        bootstrap(&fixture.0, wrong_version, &refused, false),
        "version mismatch",
    );
    assert!(!refused.exists());

    for offline in [false, true] {
        let prefix = fixture.0.join(if offline {
            "offline install ' 雪"
        } else {
            "base install ' 雪"
        });
        success(bootstrap(&fixture.0, version, &prefix, offline));
        let selected = selected_binary(&prefix, version);
        assert_eq!(
            sha256_hex(&fs::read(&selected).unwrap()),
            sha256_hex(&bytes)
        );
        let output = Command::new(&selected).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), version);
        success(output);
        let marker = fs::read(prefix.join(".native-install.json")).unwrap();
        failure(
            bootstrap(&fixture.0, version, &prefix, offline),
            "prefix already exists",
        );
        assert_eq!(
            fs::read(prefix.join(".native-install.json")).unwrap(),
            marker
        );
        assert_eq!(
            sha256_hex(&fs::read(&selected).unwrap()),
            sha256_hex(&bytes)
        );
    }
}
