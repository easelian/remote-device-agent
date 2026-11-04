#![cfg(any(target_os = "linux", target_os = "macos", windows))]

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

#[cfg(unix)]
fn command_directory(prefix: &Path) -> PathBuf {
    prefix.to_owned()
}

fn bootstrap(directory: &Path, version: &str, prefix: &Path, offline: bool) -> Output {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
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
            "& $env:RDH_TEST_INSTALLER -Offline $env:RDH_TEST_BINARY"
        } else {
            "& $env:RDH_TEST_INSTALLER -Version $env:RDH_TEST_VERSION -Base $env:RDH_TEST_BASE"
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
    command
        .env("RDH_INSTALL_DIR", prefix)
        .env("RDH_NO_START", "1")
        .env("RDH_NO_MODIFY_PATH", "1")
        .current_dir(directory)
        .output()
        .unwrap()
}

fn bootstrap_defaults_command(directory: &Path, prefix: &Path, release_version: &str) -> Command {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"));
    #[cfg(unix)]
    let mut command = {
        use std::os::unix::fs::PermissionsExt;
        let bin = directory.join("mock-release-api");
        fs::create_dir_all(&bin).unwrap();
        let curl = bin.join("curl");
        fs::write(
            &curl,
            r#"#!/bin/sh
for argument do
    if [ "$argument" = 'https://api.github.com/repos/easelian/remote-device-agent/releases?per_page=1' ]; then
        printf '[{"tag_name":"v%s","draft":false}]\n' "$RDH_TEST_RELEASE_VERSION"
        exit 0
    fi
done
exit 1
"#,
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut command = Command::new("/bin/sh");
        command.env("PATH", std::env::join_paths(paths).unwrap());
        command.stdin(fs::File::open(repository.join("packaging/install.sh")).unwrap());
        #[cfg(target_os = "linux")]
        command.env(
            "RDH_LIBC",
            if cfg!(target_env = "musl") {
                "musl"
            } else {
                "gnu"
            },
        );
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
            "-Command",
            r#"function Invoke-RestMethod {
                param($Uri, $Headers, $TimeoutSec)
                if ($Uri -ne 'https://api.github.com/repos/easelian/remote-device-agent/releases?per_page=1') { throw 'Unexpected release API request' }
                [pscustomobject]@{ tag_name = ('v' + $env:RDH_TEST_RELEASE_VERSION); draft = $false }
            }
            $rdhTestRegistry = $null
            if ($env:RDH_TEST_USER_PATH) {
                $rdhTestRegistry = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
                $rdhTestSavedPath = $rdhTestRegistry.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                if ($null -ne $rdhTestSavedPath) { $rdhTestSavedKind = $rdhTestRegistry.GetValueKind('Path') }
            }
            try {
                if ($rdhTestRegistry) {
                    $rdhTestTail = '%SystemRoot%\rdh-fixture;' + $rdhTestSavedPath
                    $rdhTestInitial = $rdhTestTail
                    if ($env:RDH_TEST_USER_PATH -eq 'existing') { $rdhTestInitial = $env:RDH_INSTALL_DIR + ';' + $rdhTestTail }
                    $rdhTestRegistry.SetValue('Path', $rdhTestInitial, [Microsoft.Win32.RegistryValueKind]::ExpandString)
                }
                Invoke-Expression (Get-Content -Raw -LiteralPath $env:RDH_TEST_INSTALLER)
                if ($rdhTestRegistry) {
                    $rdhTestStoredPath = $rdhTestRegistry.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                    if (@($rdhTestStoredPath -split ';' | Where-Object { $_ -ieq $env:RDH_INSTALL_DIR }).Count -ne 1) { throw 'User PATH must contain the installation exactly once' }
                    if (!$rdhTestStoredPath.EndsWith(';' + $rdhTestTail)) { throw 'Existing user PATH was changed' }
                    if ($rdhTestRegistry.GetValueKind('Path') -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) { throw 'The expandable PATH type was changed' }
                    if (($env:PATH -split ';')[0] -ine $env:RDH_INSTALL_DIR) { throw 'Current process PATH was not updated' }
                    if ((rdh --version) -ne $env:RDH_TEST_RELEASE_VERSION) { throw 'The installed command was not found through PATH' }
                }
            } finally {
                if ($rdhTestRegistry) {
                    if ($null -eq $rdhTestSavedPath) { $rdhTestRegistry.DeleteValue('Path', $false) }
                    else { $rdhTestRegistry.SetValue('Path', $rdhTestSavedPath, $rdhTestSavedKind) }
                    $rdhTestRegistry.Dispose()
                    if ('RdhInstaller.EnvironmentNotification' -as [type]) {
                        $rdhTestNotification = [UIntPtr]::Zero
                        [void][RdhInstaller.EnvironmentNotification]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 1000, [ref]$rdhTestNotification)
                    }
                }
            }"#,
        ]);
        command.env(
            "RDH_TEST_INSTALLER",
            repository.join("packaging/install.ps1"),
        );
        command
    };
    command
        .env("RDH_BASE", directory)
        .env("RDH_INSTALL_DIR", prefix)
        .env("RDH_TEST_RELEASE_VERSION", release_version)
        .env("RDH_NO_START", "1")
        .env("RDH_NO_MODIFY_PATH", "1")
        .env_remove("RDH_TEST_USER_PATH")
        .env_remove("RDH_VERSION")
        .current_dir(directory);
    command
}

fn bootstrap_defaults(directory: &Path, prefix: &Path, release_version: &str) -> Output {
    bootstrap_defaults_command(directory, prefix, release_version)
        .output()
        .unwrap()
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

fn selected_binary(prefix: &Path, _version: &str) -> PathBuf {
    prefix.join(if cfg!(windows) { "rdh.exe" } else { "rdh" })
}

#[test]
fn bootstrap_accepts_script_input_without_arguments() {
    let fixture = Fixture::new();
    let version = env!("CARGO_PKG_VERSION");
    let bytes = fs::read(initial_binary::path()).unwrap();
    artifact(&fixture.0, version, &bytes);
    let prefix = fixture.0.join("default install ' 雪");
    let different_release = "9999.0.0-release-fixture";
    artifact(&fixture.0, different_release, &bytes);
    failure(
        bootstrap_defaults(&fixture.0, &prefix, different_release),
        "version mismatch",
    );
    assert!(!prefix.exists());
    success(bootstrap_defaults(&fixture.0, &prefix, version));
    assert_eq!(fs::read(selected_binary(&prefix, version)).unwrap(), bytes);
    assert_eq!(fs::read_dir(&prefix).unwrap().count(), 1);
    let output = Command::new(selected_binary(&prefix, version))
        .arg("--version")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), version);
    success(output);
    failure(
        bootstrap_defaults(&fixture.0, &prefix, version),
        "command path already exists",
    );
}

#[cfg(unix)]
#[test]
fn bootstrap_persists_a_quoted_nonduplicating_shell_path() {
    let fixture = Fixture::new();
    let version = env!("CARGO_PKG_VERSION");
    artifact(
        &fixture.0,
        version,
        &fs::read(initial_binary::path()).unwrap(),
    );
    for shell in ["sh", "bash", "zsh", "fish"] {
        if Command::new(shell).arg("--version").output().is_err() {
            #[cfg(all(target_os = "linux", target_env = "gnu", target_arch = "x86_64"))]
            assert!(
                std::env::var_os("CI").is_none(),
                "CI must install the {shell} startup test tool"
            );
            eprintln!("Skipping unavailable shell: {shell}");
            continue;
        }
        let prefix = fixture.0.join(format!(
            "{shell} ' $(touch INJECTED) `touch ALSO_INJECTED` 雪"
        ));
        let bin = command_directory(&prefix);
        let rc = fixture.0.join(format!("{shell}.rc"));
        fs::write(&rc, "# Preserve this existing configuration").unwrap();
        success(
            bootstrap_defaults_command(&fixture.0, &prefix, version)
                .env("RDH_NO_MODIFY_PATH", "0")
                .env("RDH_SHELL_RC", &rc)
                .env("SHELL", shell)
                .output()
                .unwrap(),
        );
        let saved = fs::read(&rc).unwrap();
        assert!(saved.starts_with(b"# Preserve this existing configuration\n"));
        let script = if shell == "fish" {
            "source $argv[1]; source $argv[1]; command -s rdh; rdh --version; string join : $PATH"
        } else {
            ". \"$1\"; . \"$1\"; command -v rdh; rdh --version; printf '%s\\n' \"$PATH\""
        };
        let mut fresh_shell = Command::new(shell);
        match shell {
            "fish" => {
                fresh_shell.arg("--no-config");
            }
            "zsh" => {
                fresh_shell.arg("-f");
            }
            "bash" => {
                fresh_shell.args(["--noprofile", "--norc"]);
            }
            _ => {}
        }
        fresh_shell.env_remove("BASH_ENV").env_remove("ENV");
        fresh_shell.arg("-c").arg(script);
        if shell != "fish" {
            fresh_shell.arg("bootstrap-path-test");
        }
        let output = fresh_shell
            .arg(&rc)
            .current_dir(&fixture.0)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        success(output);
        let lines: Vec<_> = stdout.lines().collect();
        assert_eq!(lines[0], bin.join("rdh").to_str().unwrap());
        assert_eq!(lines[1], version);
        assert_eq!(
            lines[2]
                .split(':')
                .filter(|entry| *entry == bin.to_str().unwrap())
                .count(),
            1
        );
        assert!(!fixture.0.join("INJECTED").exists());
        assert!(!fixture.0.join("ALSO_INJECTED").exists());
        failure(
            bootstrap_defaults_command(&fixture.0, &prefix, version)
                .env("RDH_NO_MODIFY_PATH", "0")
                .env("RDH_SHELL_RC", &rc)
                .env("SHELL", shell)
                .output()
                .unwrap(),
            "command path already exists",
        );
        assert_eq!(fs::read(&rc).unwrap(), saved);
    }
}

#[cfg(unix)]
#[test]
fn bootstrap_preserves_unrelated_command_paths_before_installation() {
    use std::os::unix::fs::{MetadataExt, symlink};
    let fixture = Fixture::new();
    let version = env!("CARGO_PKG_VERSION");
    artifact(
        &fixture.0,
        version,
        &fs::read(initial_binary::path()).unwrap(),
    );
    let unrelated = fixture.0.join("unrelated program");
    fs::write(&unrelated, "keep this program").unwrap();
    for kind in ["file", "directory", "symlink", "dangling symlink"] {
        let prefix = fixture.0.join(format!("conflicting {kind}"));
        let bin = command_directory(&prefix);
        fs::create_dir(&bin).unwrap();
        let entry = bin.join("rdh");
        match kind {
            "file" => fs::write(&entry, "keep this entry").unwrap(),
            "directory" => {
                fs::create_dir(&entry).unwrap();
                fs::write(entry.join("keep"), "keep this entry").unwrap();
            }
            "symlink" => symlink(&unrelated, &entry).unwrap(),
            _ => symlink(fixture.0.join("missing program"), &entry).unwrap(),
        }
        let before = fs::symlink_metadata(&entry).unwrap();
        failure(
            bootstrap_defaults(&fixture.0, &prefix, version),
            "command path",
        );
        let after = fs::symlink_metadata(&entry).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert!(prefix.exists());
        assert!(fs::symlink_metadata(bin.join("remote-device-agent")).is_err());
        assert!(fs::symlink_metadata(bin.join("remote-device-hub")).is_err());
        match kind {
            "file" => assert_eq!(fs::read_to_string(&entry).unwrap(), "keep this entry"),
            "directory" => assert_eq!(
                fs::read_to_string(entry.join("keep")).unwrap(),
                "keep this entry"
            ),
            "symlink" => assert_eq!(fs::read_link(&entry).unwrap(), unrelated),
            _ => assert_eq!(
                fs::read_link(&entry).unwrap(),
                fixture.0.join("missing program")
            ),
        }
    }
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep this program");
}

#[cfg(windows)]
#[test]
#[ignore = "temporarily changes the user PATH; run only on a disposable Windows host"]
fn native_windows_bootstrap_persists_path_and_updates_the_current_shell() {
    let fixture = Fixture::new();
    let version = env!("CARGO_PKG_VERSION");
    artifact(
        &fixture.0,
        version,
        &fs::read(initial_binary::path()).unwrap(),
    );
    for state in ["absent", "existing"] {
        let prefix = fixture.0.join(format!("user PATH {state} ' 雪"));
        success(
            bootstrap_defaults_command(&fixture.0, &prefix, version)
                .env("RDH_NO_MODIFY_PATH", "0")
                .env("RDH_TEST_USER_PATH", state)
                .output()
                .unwrap(),
        );
    }
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

    let sidecar_prefix = fixture.0.join("per-binary checksum install");
    success(bootstrap(&fixture.0, version, &sidecar_prefix, true));
    assert_eq!(
        sha256_hex(&fs::read(selected_binary(&sidecar_prefix, version)).unwrap()),
        sha256_hex(&bytes)
    );

    let sums = fixture.0.join("SHA256SUMS");
    let valid_record = format!("{}  {}\n", sha256_hex(&bytes), artifact_name(version));
    let duplicate = format!("{valid_record}{valid_record}");
    fs::write(&sums, duplicate).unwrap();
    failure(
        bootstrap(&fixture.0, version, &refused, false),
        "checksum record",
    );
    assert!(!refused.exists());
    fs::write(
        &sums,
        format!("{}  {}\n", "0".repeat(64), artifact_name(version)),
    )
    .unwrap();
    failure(
        bootstrap(&fixture.0, version, &refused, true),
        "checksum mismatch",
    );
    assert!(!refused.exists());
    fs::write(
        &sums,
        format!("{}  other-file\n{valid_record}", "a".repeat(64)),
    )
    .unwrap();

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
        failure(
            bootstrap(&fixture.0, version, &prefix, offline),
            "command path already exists",
        );
        assert_eq!(fs::read_dir(&prefix).unwrap().count(), 1);
        assert_eq!(
            sha256_hex(&fs::read(&selected).unwrap()),
            sha256_hex(&bytes)
        );
    }
}

#[cfg(unix)]
#[test]
fn bootstrap_without_a_terminal_installs_and_defers_directory_selection() {
    use std::os::unix::process::CommandExt;
    let fixture = Fixture::new();
    let version = env!("CARGO_PKG_VERSION");
    let script = format!(
        "#!/bin/sh\nif [ \"${{1-}}\" = --version ]; then printf '%s\\n' '{version}'; exit 0; fi\npwd > launched-from\nexit 42\n"
    );
    artifact(&fixture.0, version, script.as_bytes());
    let prefix = fixture.0.join("launch");
    let mut command = bootstrap_defaults_command(&fixture.0, &prefix, version);
    command.env("RDH_NO_START", "0");
    // SAFETY: setsid is async-signal-safe; the child detaches solely so the
    // installer cannot obtain the test runner's controlling terminal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let output = command.output().unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("Run rdh in an interactive terminal"));
    success(output);
    assert!(selected_binary(&prefix, version).is_file());
    assert!(!fixture.0.join("launched-from").exists());
}
