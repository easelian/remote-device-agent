use super::*;
use std::time::Instant;

#[test]
#[ignore = "private inherited-channel entry for process ownership tests"]
fn helper_entry() {
    bootstrap().unwrap();
}

struct Fixture {
    directory: PathBuf,
    unrelated: Process,
}
impl Fixture {
    fn new() -> Self {
        super::super::linux::prepare().unwrap();
        let directory = std::env::temp_dir().join(format!(
            "rdh-cdp-owner-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        crate::storage::create_private_dir(&directory).unwrap();
        let unrelated = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        Self {
            directory,
            unrelated,
        }
    }
    fn launch(&self) -> super::super::super::Launch {
        // Both helpers leave their ancestor's group. The first survives the
        // browser, and its child is adopted only after the first is killed.
        super::super::super::Launch {
            arguments:vec!["-c".into(),r#"echo $$ > browser.pid
setsid /bin/sh -c 'echo $$ > detached.pid; setsid /bin/sh -c "echo \$\$ > nested.pid; exec sleep 60" & wait' &
while [ ! -e finish ]; do sleep 0.01; done
"#.into()],
            child_descriptors:(3,4),
        }
    }
    async fn start(&self) -> Child {
        let (mut child, read, write) =
            Child::spawn(Path::new("/bin/sh"), &self.directory, &self.launch()).unwrap();
        crate::storage::write_private_new(
            &self.directory.join("owner.json"),
            &serde_json::to_vec(&child.owner()).unwrap(),
        )
        .unwrap();
        child.start().unwrap();
        drop((read, write));
        child
    }
    async fn identities(&self) -> Vec<(u32, String)> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let pids: Option<Vec<u32>> = ["browser.pid", "detached.pid", "nested.pid"]
                .into_iter()
                .map(|name| {
                    fs::read_to_string(self.directory.join(name))
                        .ok()?
                        .trim()
                        .parse()
                        .ok()
                })
                .collect();
            if let Some(pids) = pids {
                let identities: Option<Vec<_>> = pids
                    .into_iter()
                    .map(|pid| Some((pid, crate::process_identity::started(pid).unwrap()?)))
                    .collect();
                if let Some(identities) = identities {
                    return identities;
                }
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not start: {}",
                self.directory.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn assert_gone(&mut self, identities: &[(u32, String)]) {
        for (pid, start) in identities {
            if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
                let ticks = stat
                    .rsplit_once(") ")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .nth(19)
                    .unwrap();
                assert!(
                    !start.ends_with(&format!(":{ticks}")),
                    "owned process {pid} remains, including a zombie"
                );
            }
        }
        assert!(
            self.unrelated.try_wait().unwrap().is_none(),
            "unrelated process was stopped"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.unrelated.kill();
        let _ = self.unrelated.wait();
        if !std::thread::panicking() {
            fs::remove_dir_all(&self.directory).unwrap();
        }
    }
}
async fn stopped(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.stopped().unwrap() {
            return;
        }
        assert!(Instant::now() < deadline, "supervisor failed to finish");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn browser_exit_reaps_helpers_outside_its_group() {
    let mut fixture = Fixture::new();
    let mut child = fixture.start().await;
    let identities = fixture.identities().await;
    assert_ne!(unsafe { libc::getpgid(identities[0].0 as i32) }, unsafe {
        libc::getpgid(identities[1].0 as i32)
    });
    assert_ne!(unsafe { libc::getpgid(identities[1].0 as i32) }, unsafe {
        libc::getpgid(identities[2].0 as i32)
    });
    fs::write(fixture.directory.join("finish"), []).unwrap();
    stopped(&mut child).await;
    fixture.assert_gone(&identities);
    assert!(retired(&fixture.directory, Some(&child.owner())).unwrap());
}

#[tokio::test]
async fn lifeline_loss_stops_the_entire_owned_tree() {
    let mut fixture = Fixture::new();
    let mut child = fixture.start().await;
    let identities = fixture.identities().await;
    child.force_stop();
    stopped(&mut child).await;
    fixture.assert_gone(&identities);
    assert!(completed(&fixture.directory, &child.owner()).unwrap());
}

#[tokio::test]
async fn missing_or_wrong_completion_proof_cannot_retire_a_session() {
    let fixture = Fixture::new();
    let mut child = fixture.start().await;
    fixture.identities().await;
    let owner = child.owner();
    assert!(!retired(&fixture.directory, Some(&owner)).unwrap());
    child.force_stop();
    stopped(&mut child).await;
    let mut wrong = owner.clone();
    wrong["nonce"] = "0".repeat(32).into();
    assert!(!retired(&fixture.directory, Some(&wrong)).unwrap());
    fs::remove_file(fixture.directory.join(COMPLETE)).unwrap();
    assert!(!retired(&fixture.directory, Some(&owner)).unwrap());
    let legacy = json!({"version":1,"pid":owner["pid"],"start":owner["start"]});
    assert!(retired(&fixture.directory, Some(&legacy)).is_err());
}

#[test]
#[ignore = "fixture owner killed by the parent to verify lifeline cleanup"]
fn owner_entry() {
    let directory = PathBuf::from(std::env::var_os("RDH_SUPERVISOR_FIXTURE_DIR").unwrap());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Fixture {
            directory,
            unrelated: Command::new("/bin/true").spawn().unwrap(),
        };
        let child = fixture.start().await;
        fixture.identities().await;
        // The test parent kills this owner; neither Drop nor graceful close runs.
        std::thread::sleep(Duration::from_secs(60));
        drop(child);
    });
}

#[tokio::test]
async fn killed_agent_owner_leaves_a_recoverable_completion_proof() {
    let mut fixture = Fixture::new();
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "browser::process::posix::supervisor::tests::owner_entry",
            "--ignored",
            "--test-threads=1",
        ])
        .env("RDH_SUPERVISOR_FIXTURE_DIR", &fixture.directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let identities = fixture.identities().await;
    let record: Value =
        serde_json::from_slice(&fs::read(fixture.directory.join("owner.json")).unwrap()).unwrap();
    owner.kill().unwrap();
    owner.wait().unwrap();
    let supervisor = record["pid"].as_u64().unwrap() as i32;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let reaped = unsafe { libc::waitpid(supervisor, std::ptr::null_mut(), libc::WNOHANG) };
        if reaped == supervisor {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not finish after Agent SIGKILL"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fixture.assert_gone(&identities);
    assert!(retired(&fixture.directory, Some(&record)).unwrap());
}

#[tokio::test]
async fn termination_signal_finishes_reaping_before_publishing_completion() {
    let mut fixture = Fixture::new();
    let mut child = fixture.start().await;
    let identities = fixture.identities().await;
    // The direct child is unreaped, so its PID cannot be reused here.
    assert!(!exited(child.process.id()).unwrap());
    assert_eq!(
        unsafe { libc::kill(child.process.id() as i32, libc::SIGTERM) },
        0
    );
    stopped(&mut child).await;
    fixture.assert_gone(&identities);
    assert!(retired(&fixture.directory, Some(&child.owner())).unwrap());
}
