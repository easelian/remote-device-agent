#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-processes"),
    all(windows, feature = "experimental-windows-processes")
))]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::{Duration, Instant};

use rdh_agent::{config::DeviceConfig, files::Files, processes::Processes, random};
use serde_json::{Value, json};

struct Fixture {
    root: PathBuf,
    processes: Processes,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.processes.close(Duration::from_secs(5)) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

impl Fixture {
    fn call(&self, operation: &str, params: Value) -> Result<Value, String> {
        self.processes
            .dispatch(
                operation,
                &params,
                "fixture-request",
                Instant::now() + Duration::from_secs(15),
                &AtomicBool::new(false),
            )
            .map_err(|error| error.message)
    }

    fn start(&self, command: &str, wait: f64) -> String {
        self.call(
            "start_process",
            json!({"root":"work","command":command,"wait_seconds":wait}),
        )
        .unwrap()["job_id"]
            .as_str()
            .unwrap()
            .into()
    }

    fn complete(&self, id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(if cfg!(windows) { 65 } else { 8 });
        loop {
            let result = self.call("read_process", json!({"job_id":id})).unwrap();
            if result["status"] != "running" && result["cleanup_pending"] == false {
                return normalized(result);
            }
            assert!(Instant::now() < deadline, "fixture process did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read(&self, params: Value) -> Value {
        match self.call("read_process", params) {
            Ok(result) => normalized(result),
            Err(message) => json!({"failure":message}),
        }
    }
}

fn normalized(mut result: Value) -> Value {
    let object = result.as_object_mut().unwrap();
    object.remove("job_id");
    object.remove("pid");
    object.remove("started_at");
    normalize_cursor(&mut result);
    result
}

fn normalize_cursor(result: &mut Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    // JSON has one number type; compare the JavaScript Number values rather
    // than serde_json's different integer and floating-point representations.
    for field in ["offset", "next_offset"] {
        if let Some(value) = object.get(field).and_then(Value::as_f64) {
            object.insert(field.into(), json!(value));
        }
    }
}

#[test]
#[ignore = "requires a bundled legacy Node process fixture; run explicitly"]
fn legacy_and_rust_process_cursors_and_input_match() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_PROCESS_HELPER")
            .expect("RDH_LEGACY_PROCESS_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-legacy-process-{}",
        random::hex_token::<12>().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let roots = rdh_agent::config::local_roots(&[format!("work={}", root.display())]).unwrap();
    let config = DeviceConfig {
        gateway_url: "http://localhost".into(),
        device_id: "fixture".into(),
        token: "fixture".into(),
        roots: roots.clone(),
        allow_shell: true,
        allow_write: true,
        shell: if cfg!(windows) {
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
                .to_str()
                .unwrap()
                .into()
        } else {
            "/bin/sh".into()
        },
        max_processes: 4,
        max_process_seconds: if cfg!(windows) { 60 } else { 10 },
    };
    let files = Arc::new(Files::new(&config.roots, true).unwrap());
    let fixture = Fixture {
        root,
        processes: Processes::new(&config, files, vec![], None).unwrap(),
    };
    let node = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| "node".into());
    let mut child = Command::new(node)
        .arg(&helper)
        .arg(&roots["work"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Three sequential shell launches; native Windows ARM64 previously took
    // about 25 s per PowerShell startup. Keep each job bounded at 60 s.
    let deadline = Instant::now() + Duration::from_secs(if cfg!(windows) { 200 } else { 20 });
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("legacy process fixture timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "legacy fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut legacy: Value = serde_json::from_slice(&output.stdout).unwrap();
    for result in legacy["cursors"].as_object_mut().unwrap().values_mut() {
        normalize_cursor(result);
    }
    for field in ["echoed", "dropped", "tail"] {
        normalize_cursor(&mut legacy[field]);
    }

    let (text_command, input_command, overflow_command) = if cfg!(windows) {
        (
            "$b=[Text.Encoding]::UTF8.GetBytes('A😀B'+[char]10+'last'); [Console]::OpenStandardOutput().Write($b,0,$b.Length); exit 7",
            "[Console]::OpenStandardInput().CopyTo([Console]::OpenStandardOutput())",
            "$b=[Text.Encoding]::UTF8.GetBytes(('x' * 262150)); [Console]::OpenStandardOutput().Write($b,0,$b.Length)",
        )
    } else {
        (
            "printf 'A😀B\\nlast'; exit 7",
            "dd bs=1 count=7 2>/dev/null",
            "dd if=/dev/zero bs=262150 count=1 2>/dev/null | tr '\\000' x",
        )
    };
    let id = fixture.start(text_command, 2.0);
    fixture.complete(&id);
    let cursors = json!({
        "full": fixture.read(json!({"job_id":id})),
        "emoji": fixture.read(json!({"job_id":id,"offset":1,"limit":1})),
        "end": fixture.read(json!({"job_id":id,"offset":30,"limit":2})),
        "large": fixture.read(json!({"job_id":id,"offset":9_007_199_254_740_992_u64,"limit":1})),
        "huge": fixture.read(json!({"job_id":id,"offset":1e100,"limit":1})),
        "maximum": fixture.read(json!({"job_id":id,"offset":f64::MAX,"limit":1})),
        "negative": fixture.read(json!({"job_id":id,"offset":-1})),
        "fractional": fixture.read(json!({"job_id":id,"offset":0.5})),
        "null_offset": fixture.read(json!({"job_id":id,"offset":null})),
        "null_limit": fixture.read(json!({"job_id":id,"offset":0,"limit":null})),
        "zero_limit": fixture.read(json!({"job_id":id,"offset":0,"limit":0})),
        "unknown": fixture.read(json!({"job_id":"unknown"})),
    });
    let cat = fixture.start(input_command, 0.0);
    let input = normalized(
        fixture
            .call(
                "write_process_stdin",
                json!({"job_id":cat,"text":"go😀\n","close":true}),
            )
            .unwrap(),
    );
    let echoed = fixture.complete(&cat);
    let overflow = fixture.start(overflow_command, 3.0);
    fixture.complete(&overflow);
    let dropped = fixture.read(json!({"job_id":overflow,"offset":0,"limit":1}));
    let tail = fixture.read(json!({"job_id":overflow,"offset":262149,"limit":2}));
    assert!(fixture.processes.close(Duration::from_secs(5)));
    let actual =
        json!({"cursors":cursors,"input":input,"echoed":echoed,"dropped":dropped,"tail":tail});
    let mut differences = Vec::new();
    for key in ["cursors", "input", "echoed", "dropped", "tail"] {
        if actual[key] != legacy[key] {
            differences.push(format!(
                "{key}: legacy={} native={}",
                legacy[key], actual[key]
            ));
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}
