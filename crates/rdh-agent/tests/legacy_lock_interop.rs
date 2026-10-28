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
}

impl Process {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
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
        Self { child, lines }
    }

    fn expect_line(&self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(remaining)
                .unwrap_or_else(|error| panic!("fixture did not report {expected:?}: {error}"));
            // The Rust test harness may print its test name on the same line.
            if line.trim_end().ends_with(expected) {
                return;
            }
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
                return status;
            }
            assert!(Instant::now() < deadline, "fixture did not exit");
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
    Process::spawn(Command::new(executable).arg(helper).arg(action).arg(path))
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
