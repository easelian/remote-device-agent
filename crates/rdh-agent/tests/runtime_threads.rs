#![cfg(target_os = "linux")]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn waiting_for_pairing_does_not_create_one_scheduler_thread_per_cpu() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "rdh-thread-bound-{}",
        rdh_agent::random::hex_token::<8>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    fs::set_permissions(&fixture.0, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "work", "empty"] {
        fs::create_dir(fixture.0.join(name)).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let root = format!("work={}", fixture.0.join("work").display());
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_rdh"))
            .env_clear()
            .env("PATH", fixture.0.join("empty"))
            .env("HOME", fixture.0.join("home"))
            .env("XDG_CONFIG_HOME", fixture.0.join("config"))
            .env("XDG_STATE_HOME", fixture.0.join("state"))
            .args([
                "--server",
                &server,
                "--root",
                &root,
                "--no-browser",
                "--pair-only",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let _connection = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("local pairing listener failed: {error}"),
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "Agent exited before pairing"
        );
        assert!(
            Instant::now() < deadline,
            "Agent did not reach the local listener"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        let status = fs::read_to_string(format!("/proc/{}/status", child.0.id())).unwrap();
        let caught = status
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
            .unwrap_or(0);
        if caught & (1u64 << (libc::SIGTERM - 1)) != 0 {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Agent did not install its shutdown handler"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let threads: usize = status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Scheduler workers are capped at four; allow extra runtime helper threads.
    assert!(threads <= 8, "idle pairing spawned {threads} threads");
    assert_eq!(unsafe { libc::kill(child.0.id() as i32, libc::SIGTERM) }, 0);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "Agent did not stop cleanly");
            break;
        }
        assert!(Instant::now() < deadline, "Agent did not stop");
        std::thread::sleep(Duration::from_millis(10));
    }
}
