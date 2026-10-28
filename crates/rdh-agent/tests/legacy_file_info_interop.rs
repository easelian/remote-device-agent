#![cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files")
))]

use std::collections::BTreeMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rdh_agent::{files::Files, random};
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires a bundled legacy Node file fixture; run explicitly"]
fn legacy_and_rust_file_fingerprints_match() {
    let helper = PathBuf::from(
        std::env::var_os("RDH_LEGACY_FILE_HELPER")
            .expect("RDH_LEGACY_FILE_HELPER must name the bundled legacy fixture"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    let fixture = Fixture(std::env::temp_dir().canonicalize().unwrap().join(format!(
        "rdh-legacy-file-{}",
        random::hex_token::<12>().unwrap()
    )));
    fs::create_dir(&fixture.0).unwrap();
    fs::write(fixture.0.join("file"), "test file 😀").unwrap();
    let file = fs::File::open(fixture.0.join("file")).unwrap();
    let files = Files::new(
        &BTreeMap::from([("work".into(), fixture.0.to_str().unwrap().into())]),
        false,
    )
    .unwrap();
    let writable = Files::new(
        &BTreeMap::from([("work".into(), fixture.0.to_str().unwrap().into())]),
        true,
    )
    .unwrap();
    let params = json!({"root":"work","path":"file"});

    for (seconds, nanoseconds) in [
        (1_737_982_632, 508_156_297),
        (1_766_863_886, 76_156_279),
        (1_700_000_000, 0),
        (-1, 999_999_999),
    ] {
        let times = [
            libc::timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
            libc::timespec {
                tv_sec: seconds,
                tv_nsec: nanoseconds,
            },
        ];
        // SAFETY: file is open and times contains the two required timespecs.
        assert_eq!(
            unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) },
            0
        );
        let mut rust = json!({
            "info": files.dispatch("file_info", &params).unwrap(),
            "chunk": files.dispatch("read_file_chunk", &params).unwrap(),
            "batch": files.dispatch("read_multiple_files", &json!({"root":"work","paths":["file"]})).unwrap(),
            "list_large_offset": files.dispatch("list_directory", &json!({"root":"work","offset":1e100})).unwrap(),
            "invalid": {
                "read_offset": files.dispatch("read_file", &json!({"root":"work","path":"file","offset":-1})).unwrap_err(),
                "read_length": files.dispatch("read_file", &json!({"root":"work","path":"file","length":0})).unwrap_err(),
                "chunk_encoding": files.dispatch("read_file_chunk", &json!({"root":"work","path":"file","encoding":0})).unwrap_err(),
                "chunk_from_end": files.dispatch("read_file_chunk", &json!({"root":"work","path":"file","from_end":null})).unwrap_err(),
                "root": files.dispatch("read_file", &json!({"root":null,"path":null})).unwrap_err(),
                "path": files.dispatch("read_file", &json!({"root":"work","path":null})).unwrap_err(),
                "chunk_empty": files.dispatch("read_file_chunk", &json!({"root":"work","path":"."})).unwrap_err(),
                "batch_range": files.dispatch("read_multiple_files", &json!({"root":"work","paths":["file"],"length":0})).unwrap(),
                "batch_empty": files.dispatch("read_multiple_files", &json!({"root":"work","paths":["."]})).unwrap(),
                "list_offset": files.dispatch("list_directory", &json!({"root":"work","offset":-1})).unwrap_err(),
                "write_disabled": files.dispatch("write_file", &json!({"root":null,"path":null,"content":null})).unwrap_err(),
                "write_content": writable.dispatch("write_file", &json!({"root":"work","path":"file","content":null})).unwrap_err(),
                "write_expected": writable.dispatch("write_file", &json!({"root":"work","path":"file","content":"new","expected_sha256":null})).unwrap_err(),
                "edit_empty": writable.dispatch("edit_file", &json!({"root":"work","path":"file","old_text":0,"new_text":"new"})).unwrap_err(),
                "edit_type": writable.dispatch("edit_file", &json!({"root":"work","path":"file","old_text":{},"new_text":"new"})).unwrap_err(),
                "mkdir_path": writable.dispatch("make_directory", &json!({"root":"work","path":null})).unwrap_err(),
                "move_type": writable.dispatch("move_file", &json!({"root":"work","source":null,"destination":"file"})).unwrap_err(),
                "search_query": files.dispatch("search_files", &json!({"root":"work","query":null})).unwrap_err(),
            }
        });
        let node = std::env::var_os("RDH_TEST_NODE").unwrap_or_else(|| "node".into());
        let mut child = Command::new(node)
            .arg(&helper)
            .arg(&fixture.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("legacy file fixture timed out");
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
        // JSON has one number type, while serde_json distinguishes an integer
        // spelling from an equivalent floating-point spelling.
        let rust_mtime = rust["info"]["mtime_ms"].as_f64().unwrap();
        let legacy_mtime = legacy["info"]["mtime_ms"].as_f64().unwrap();
        assert_eq!(
            rust_mtime, legacy_mtime,
            "timestamp {seconds}.{nanoseconds:09}"
        );
        rust["info"].as_object_mut().unwrap().remove("mtime_ms");
        legacy["info"].as_object_mut().unwrap().remove("mtime_ms");
        assert_eq!(rust, legacy, "timestamp {seconds}.{nanoseconds:09}");
        assert_eq!(
            fs::read(fixture.0.join("file")).unwrap(),
            "test file 😀".as_bytes()
        );
    }
}
