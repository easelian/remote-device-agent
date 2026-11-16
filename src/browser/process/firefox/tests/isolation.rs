use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn client_worker() {
    let Some(port) = std::env::var("RDH_D2B_PROBE_PORT")
        .ok()
        .map(|s| s.parse::<u16>().unwrap())
    else {
        return;
    };
    let hidden = PathBuf::from(std::env::var_os("RDH_D2B_HIDDEN").unwrap());
    let init = std::env::var("RDH_D2B_INIT").unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/session")),
    )
    .await;
    let mut session_new = false;
    let connected = if let Ok(Ok((mut ws, _))) = result {
        ws.send(Message::Text(
            json!({"id":1,"method":"session.new","params":{"capabilities":{}}})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        if let Ok(Some(Ok(Message::Text(reply)))) =
            tokio::time::timeout(Duration::from_secs(1), ws.next()).await
        {
            session_new = serde_json::from_str::<Value>(&reply).unwrap()["type"] == "success";
        }
        true
    } else {
        false
    };
    let namespace_open = fs::File::open(format!("/proc/{init}/ns/net"));
    let namespace_entered = namespace_open
        .is_ok_and(|fd| unsafe { libc::setns(fd.as_raw_fd(), libc::CLONE_NEWNET) } == 0);
    println!(
        "D2B_CLIENT={}",
        json!({"uid":unsafe{libc::geteuid()},"connected":connected,"session_new":session_new,"private_file_readable":fs::read(&hidden).is_ok(),"namespace_entered":namespace_entered})
    );
}

async fn probe(mut command: tokio::process::Command, port: u16, hidden: &Path, init: u64) -> Value {
    command
        .env("RDH_D2B_PROBE_PORT", port.to_string())
        .env("RDH_D2B_HIDDEN", hidden)
        .env("RDH_D2B_INIT", init.to_string())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(6), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    serde_json::from_str(
        stdout
            .lines()
            .find_map(|l| l.strip_prefix("D2B_CLIENT="))
            .expect("probe result"),
    )
    .unwrap()
}

struct PublicClient(PathBuf);
impl PublicClient {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "rdh-d2b-client-{}",
            crate::random::hex_token::<8>().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let binary = directory.join("client");
        fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        Self(directory)
    }
}
impl Drop for PublicClient {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires actual Agent sandbox and sudo -n -u nobody"]
async fn first_session_isolated_from_other_uid_and_network_enabled_shell() {
    use crate::{config::DeviceConfig, policy::Policy, sandbox::Sandbox};
    let mut fixture = Fixture::new("native-isolation");
    let public = PublicClient::new();
    let work = fixture.base.join("work");
    storage::create_private_dir(&work).unwrap();
    let config_file = fixture.base.join("device.json");
    storage::write_private_new(&config_file, b"synthetic-config").unwrap();
    storage::write_private_new(
        &fixture.base.join("device.policy.json"),
        b"synthetic-policy",
    )
    .unwrap();
    let config = DeviceConfig::parse(json!({"gateway_url":"http://127.0.0.1:8111","device_id":"d2b-fixture","token":"x".repeat(32),"roots":{"work":work},"allow_shell":true,"allow_write":true})).unwrap();
    let policy = Policy::parse(
        &json!({"version":1,"sandbox":{"mode":"bwrap","network":true,"hide_home":false}}),
        &config,
    )
    .unwrap();
    let sandbox = Sandbox::prepare(&config, &policy, Some(&config_file)).unwrap();
    for iteration in 0..3 {
        let mut owner = fixture.spawn(None);
        let ready = owner.ready().await.unwrap();
        // The inherited socket is connected but session.new has not been sent.
        // Every hostile attempt precedes the trusted client's first handshake.
        let hidden = owner.directory.join("synthetic-secret");
        storage::write_private_new(&hidden, b"D2b-fixture-only").unwrap();
        let record = owner_record(&owner);
        let init = record["init_pid"].as_u64().unwrap();
        let worker = "browser::process::firefox::tests::isolation::client_worker";
        let mut other = tokio::process::Command::new("sudo");
        other
            .args(["-n", "-u", "nobody", "--", "/usr/bin/env"])
            .arg(format!("RDH_D2B_PROBE_PORT={}", ready.port))
            .arg(format!("RDH_D2B_HIDDEN={}", hidden.display()))
            .arg(format!("RDH_D2B_INIT={init}"))
            .arg(public.0.join("client"))
            .args(["--exact", worker, "--nocapture"]);
        let result = probe(other, ready.port, &hidden, init).await;
        assert_eq!(result["uid"], 65534);
        assert_eq!(result["connected"], false);
        assert_eq!(result["session_new"], false);
        assert_eq!(result["private_file_readable"], false);
        assert_eq!(result["namespace_entered"], false);
        fixture.record(json!({"kind":"first_session_access","iteration":iteration,"subject":"nobody","result":result}));
        let shell = format!(
            "exec '{}' --exact {worker} --nocapture",
            std::env::current_exe().unwrap().display()
        );
        let (program, args) = sandbox.command(&work, &shell).unwrap();
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        let result = probe(command, ready.port, &hidden, init).await;
        assert_eq!(result["connected"], false);
        assert_eq!(result["session_new"], false);
        assert_eq!(result["private_file_readable"], false);
        assert_eq!(result["namespace_entered"], false);
        fixture.record(json!({"kind":"first_session_access","iteration":iteration,"subject":"agent_sandbox","network_allowed":true,"result":result,"sandbox":sandbox.report()}));
        let mut ws = handshake(ready, false).await;
        request(&mut ws, 2, "browser.close", json!({})).await;
        owner.close(true).await.unwrap();
    }
    fixture.finish();
}
