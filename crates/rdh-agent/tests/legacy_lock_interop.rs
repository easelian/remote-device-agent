#![cfg(any(
    target_os = "linux",
    target_os = "macos",
    all(windows, feature = "experimental-windows-private-storage")
))]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use rdh_agent::{profile_lock::ProfileLocks, random};

const RUST_HOLD_CONFIG: &str = "RDH_TEST_LEGACY_LOCK_RUST_HOLD";
const TIMEOUT: Duration = Duration::from_secs(60);

struct Process {
    child: Child,
    lines: Receiver<String>,
    label: String,
    started: Instant,
    legacy_acl_commands: u32,
}

impl Process {
    fn spawn(command: &mut Command, label: &str) -> Self {
        let started = Instant::now();
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        eprintln!("Started {label}, pid={}", child.id());
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            lines,
            label: label.into(),
            started,
            legacy_acl_commands: 0,
        }
    }

    fn expect_line(&mut self, expected: &str) {
        // The real Windows implementation bounds each PowerShell ACL command
        // at 60 s, while one fixture action executes several in sequence. On
        // native ARM64 the first two successful calls took 24 s and 25 s; an
        // aggregate 60 s timeout killed the third call before lock acquisition.
        // Retain each legacy timeout and add one ordinary helper budget around
        // the bounded composition. Rust holders and Unix keep their 60 s limit.
        let commands = if cfg!(windows) && self.legacy_acl_commands != 0 {
            if expected == "released" {
                1
            } else {
                self.legacy_acl_commands
            }
        } else {
            0
        };
        let budget = TIMEOUT * (commands + 1);
        let deadline = Instant::now() + budget;
        let mut transcript = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "{} pid={} did not report {expected:?} after {:?} (budget {budget:?}): {error}; status={:?}; stdout={transcript:?}",
                    self.label,
                    self.child.id(),
                    self.started.elapsed(),
                    self.child.try_wait(),
                )
            });
            // The Rust test harness may print its test name on the same line.
            if line.trim_end().ends_with(expected) {
                eprintln!(
                    "{} reported {expected:?} after {:?}",
                    self.label,
                    self.started.elapsed()
                );
                return;
            }
            // Retain bounded diagnostics when a helper emits an unexpected line.
            if transcript.len() == 16 {
                transcript.remove(0);
            }
            transcript.push(line.chars().take(1024).collect::<String>());
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.child.stdin.as_mut().unwrap(), "{line}").unwrap();
        self.child.stdin.as_mut().unwrap().flush().unwrap();
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                eprintln!(
                    "{} exited {status} after {:?}",
                    self.label,
                    self.started.elapsed()
                );
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{} pid={} did not exit after {:?}",
                self.label,
                self.child.id(),
                self.started.elapsed()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_and_wait(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node_process(helper: &Path, action: &str, path: &Path) -> Process {
    let executable = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| OsString::from("node"));
    let mut process = Process::spawn(
        Command::new(executable)
            .arg(helper)
            .arg(action)
            .arg(path)
            .env("RDH_TEST_LEGACY_LOCK_TRACE", "1"),
        &format!("legacy {action}"),
    );
    // Counts follow the actual one-profile legacy source: guard create/check,
    // lock create/check, owner create; acquire also releases its owner. Recovery
    // checks guard, lock, and owner. No production deadline is changed.
    process.legacy_acl_commands = match action {
        "prepare" => 1,
        "hold" => 5,
        "acquire" => 6,
        "recover" => 4,
        "canonical" => 0,
        _ => panic!("unexpected legacy fixture action"),
    };
    process
}

fn node_once(helper: &Path, action: &str, path: &Path, expected: &str) {
    let mut process = node_process(helper, action, path);
    process.expect_line(expected);
    assert!(process.wait().success(), "legacy fixture {action} failed");
}

#[test]
#[ignore = "requires a bundled legacy Node lock fixture; run explicitly"]
fn legacy_node_and_rust_locks_interoperate() {
    if let Some(config) = std::env::var_os(RUST_HOLD_CONFIG) {
        let mut held = ProfileLocks::acquire(&[PathBuf::from(config)]).unwrap();
        println!("RDH_FIXTURE_HELD");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
        held.release().unwrap();
        return;
    }

    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_LOCK_HELPER")
            .expect("RDH_LEGACY_LOCK_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let temporary = std::env::temp_dir();
    #[cfg(unix)]
    let temporary = temporary.canonicalize().unwrap();
    #[cfg(windows)]
    let temporary = {
        // The legacy path checker rejects short-name aliases; Rust's native
        // canonicalizer adds an extended-path prefix the legacy API rejects.
        // Use the same Node canonical spelling that the legacy checker expects.
        let mut canonical = node_process(&helper, "canonical", &temporary);
        let line = canonical.lines.recv_timeout(TIMEOUT).unwrap();
        assert!(canonical.wait().success());
        let path: PathBuf = serde_json::from_str(&line).unwrap();
        assert!(path.is_absolute());
        path
    };
    let fixture = Fixture(temporary.join(format!(
        "rdh-legacy-lock-{}",
        random::hex_token::<12>().unwrap()
    )));
    node_once(&helper, "prepare", &fixture.0, "prepared");
    let config = fixture.0.join("device with spaces.json");
    let mut lock_name = config.as_os_str().to_os_string();
    lock_name.push(".node-lock");
    let lock_path = PathBuf::from(lock_name);

    let mut node = node_process(&helper, "hold", &config);
    node.expect_line("held");
    assert!(ProfileLocks::acquire(std::slice::from_ref(&config)).is_err());
    assert!(ProfileLocks::recover(std::slice::from_ref(&config)).is_err());
    node.send("release");
    node.expect_line("released");
    assert!(node.wait().success());

    let mut rust = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    node_once(&helper, "acquire", &config, "blocked");
    node_once(&helper, "recover", &config, "blocked");
    rust.release().unwrap();
    node_once(&helper, "acquire", &config, "acquired");
    let mut rust = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    rust.release().unwrap();

    // A dead Node owner can only be reclaimed after its process was reaped.
    let mut node = node_process(&helper, "hold", &config);
    node.expect_line("held");
    node.kill_and_wait();
    assert!(lock_path.is_dir());
    ProfileLocks::recover(std::slice::from_ref(&config)).unwrap();
    assert!(!lock_path.exists());
    let mut rust = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    rust.release().unwrap();

    // Test the reverse direction with a real Rust owner, not a forged PID.
    let mut rust_child = Process::spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "legacy_node_and_rust_locks_interoperate",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(RUST_HOLD_CONFIG, &config),
        "Rust holder",
    );
    rust_child.expect_line("RDH_FIXTURE_HELD");
    node_once(&helper, "recover", &config, "blocked");
    rust_child.kill_and_wait();
    assert!(lock_path.is_dir());
    node_once(&helper, "recover", &config, "recovered");
    assert!(!lock_path.exists());
    let mut rust = ProfileLocks::acquire(std::slice::from_ref(&config)).unwrap();
    rust.release().unwrap();
}
