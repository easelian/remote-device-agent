//! Bounded Protocol v1 file operations.
//!
//! Linux requires openat2 (Linux 5.6+). Experimental macOS uses directory-relative
//! opens with kernel NOFOLLOW_ANY plus descriptor path checks. Both retain
//! configured root identities and reject symbolic links in request paths.
//! macOS remains disabled by default pending native filesystem validation.
use serde_json::Value;
use std::collections::BTreeMap;

#[cfg(all(target_os = "macos", feature = "experimental-macos-files"))]
#[path = "files_macos.rs"]
mod macos;

pub const MUTATION_DEADLINE_EXPIRED: &str = "File mutation deadline expired before execution";
pub const MUTATION_SHUTTING_DOWN: &str = "Agent is shutting down; file mutation was not started";

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files")
))]
pub const FILE_OPERATIONS: &[&str] = &[
    "list_directory",
    "read_file",
    "file_info",
    "read_file_chunk",
    "read_multiple_files",
    "search_files",
    "write_file",
    "edit_file",
    "make_directory",
    "move_file",
];
#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files")
)))]
pub const FILE_OPERATIONS: &[&str] = &[];

pub struct Files {
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    ))]
    roots: BTreeMap<String, posix::Root>,
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    ))]
    allow_write: bool,
    // The RPC admission limit bounds callers waiting on this single lock.
    // Ownership stays with the actual blocking operation through commit/cleanup.
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    ))]
    mutation: std::sync::Mutex<()>,
}

pub(crate) struct SearchEntry {
    pub name: String,
    pub kind: &'static str,
}

#[cfg(all(test, target_os = "macos", not(feature = "experimental-macos-files")))]
mod macos_default_tests {
    use super::*;

    #[test]
    fn files_and_search_remain_unavailable_without_the_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-files-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        let roots = BTreeMap::from([("work".into(), path.to_str().unwrap().into())]);
        assert!(Files::new(&roots, true).is_err());
        assert!(FILE_OPERATIONS.is_empty());
        assert!(crate::searches::SEARCH_OPERATIONS.is_empty());
        assert!(!path.exists());
    }
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    )
))]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::json;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        path: PathBuf,
        files: Files,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_write(false)
        }
        fn with_write(allow_write: bool) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh-rust-files-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            // Use an owner before constructing Files so a failed initialization
            // still removes the fixture directory during unwinding.
            let mut fixture = Self {
                path,
                files: Files {
                    roots: BTreeMap::new(),
                    allow_write,
                    mutation: std::sync::Mutex::new(()),
                },
            };
            fixture.files = Files::new(
                &BTreeMap::from([("work".into(), fixture.path.to_str().unwrap().into())]),
                allow_write,
            )
            .unwrap();
            fixture
        }
        fn call(&self, op: &str, mut params: Value) -> Result<Value, String> {
            params["root"] = json!("work");
            self.files.dispatch(op, &params)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn utf8_bytes_bom_digest_and_strict_boundaries() {
        let fixture = Fixture::new();
        let data = "\u{feff}A😀é".as_bytes();
        fs::write(fixture.path.join("text"), data).unwrap();
        let full = fixture.call("read_file", json!({"path":"text"})).unwrap();
        assert_eq!(full["content"], "\u{feff}A😀é");
        assert_eq!(full["size_bytes"], data.len());
        assert_eq!(
            full["sha256"],
            "6638cfcdf2006d60c475dbbb5ee1a70434c5d75d34a262d64e5d4bb44cefa1cf"
        );
        let partial = fixture
            .call("read_file", json!({"path":"text","offset":4,"length":5}))
            .unwrap();
        assert_eq!(partial["content"], "😀");
        assert_eq!(partial["next_offset"], 8);
        for (offset, length) in [(5, 4), (4, 3)] {
            assert!(
                fixture
                    .call(
                        "read_file",
                        json!({"path":"text","offset":offset,"length":length})
                    )
                    .is_err()
            );
        }
        fs::write(fixture.path.join("invalid"), [0xff, 0]).unwrap();
        assert!(
            fixture
                .call("read_file", json!({"path":"invalid"}))
                .is_err()
        );
        let eof = fixture
            .call("read_file", json!({"path":"text","offset":100}))
            .unwrap();
        assert_eq!(eof["offset"], data.len());
        assert_eq!(eof["content"], "");
    }

    #[test]
    fn fingerprints_use_the_legacy_number_format() {
        for (value, expected) in [
            (0.0, "0"),
            (-0.0, "0"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (-0.0000009999999974752427, "-9.999999974752427e-7"),
            (1e20, "100000000000000000000"),
            (1e21, "1e+21"),
            (u64::MAX as f64, "18446744073709552000"),
            (
                1_737_982_632.0 * 1_000.0 + 508_156_297.0 / 1_000_000.0,
                "1737982632508.1562",
            ),
        ] {
            assert_eq!(posix::js_number(value), expected);
        }
        assert_eq!(
            posix::js_unsigned(9_007_199_254_740_993),
            "9007199254740992"
        );

        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let fixture = Fixture::new();
        let path = fixture.path.join("timestamped");
        fs::write(&path, b"test").unwrap();
        let file = fs::File::open(path).unwrap();
        let times = [
            libc::timespec {
                tv_sec: 1_737_982_632,
                tv_nsec: 508_156_297,
            },
            libc::timespec {
                tv_sec: 1_737_982_632,
                tv_nsec: 508_156_297,
            },
        ];
        // SAFETY: file is open and times contains the two required timespecs.
        assert_eq!(
            unsafe { libc::futimens(file.as_raw_fd(), times.as_ptr()) },
            0
        );
        let metadata = file.metadata().unwrap();
        if metadata.mtime_nsec() != 508_156_297 {
            eprintln!("filesystem did not retain the nanosecond test timestamp");
            return;
        }
        let info = fixture
            .call("file_info", json!({"path":"timestamped"}))
            .unwrap();
        let chunk = fixture
            .call("read_file_chunk", json!({"path":"timestamped"}))
            .unwrap();
        let fingerprint = info["fingerprint"].as_str().unwrap();
        assert!(fingerprint.ends_with(":1737982632508.1562"));
        assert_eq!(chunk["fingerprint"], info["fingerprint"]);
    }

    #[test]
    fn utf16_source_offsets_and_batch_utf8_budget() {
        let fixture = Fixture::new();
        for (name, little) in [("le", true), ("be", false)] {
            let bytes: Vec<u8> = "\u{feff}你😀好"
                .encode_utf16()
                .flat_map(|u| {
                    if little {
                        u.to_le_bytes()
                    } else {
                        u.to_be_bytes()
                    }
                })
                .collect();
            fs::write(fixture.path.join(name), &bytes).unwrap();
            let all = fixture.call("read_file", json!({"path":name})).unwrap();
            assert_eq!(all["content"], "\u{feff}你😀好");
            assert_eq!(all["content_base64"], STANDARD.encode(&bytes));
            assert_eq!(all["utf8_valid"], false);
            let part = fixture
                .call("read_file", json!({"path":name,"offset":4,"length":5}))
                .unwrap();
            assert_eq!(part["content"], "😀");
            assert_eq!(part["next_offset"], 8);
            for (offset, length) in [(3, 4), (6, 4), (4, 2)] {
                assert!(
                    fixture
                        .call(
                            "read_file",
                            json!({"path":name,"offset":offset,"length":length})
                        )
                        .is_err()
                );
            }
            let batch = fixture
                .call(
                    "read_multiple_files",
                    json!({"paths":[name],"offset":2,"total_limit":3}),
                )
                .unwrap();
            assert_eq!(batch["total_bytes"], 3);
            assert_eq!(batch["files"][0]["content"], "你");
            assert_eq!(batch["files"][0]["next_offset"], 4);
            assert_eq!(
                STANDARD
                    .decode(batch["files"][0]["content_base64"].as_str().unwrap())
                    .unwrap(),
                bytes[2..4]
            );
            assert_eq!(batch["truncated"], true);
        }
    }

    #[test]
    fn chunks_are_exact_bytes_and_tail_relative() {
        let fixture = Fixture::new();
        let bytes = "😀😀😀".as_bytes();
        fs::write(fixture.path.join("bytes"), bytes).unwrap();
        let chunk = fixture
            .call(
                "read_file_chunk",
                json!({"path":"bytes","offset":1,"length":5}),
            )
            .unwrap();
        assert_eq!(chunk["offset"], 1);
        assert_eq!(chunk["next_offset"], 6);
        assert_eq!(chunk["utf8_valid"], false);
        assert_eq!(
            STANDARD
                .decode(chunk["content_base64"].as_str().unwrap())
                .unwrap(),
            bytes[1..6]
        );
        let tail = fixture
            .call(
                "read_file_chunk",
                json!({"path":"bytes","offset":4,"length":4,"from_end":true,"encoding":"base64"}),
            )
            .unwrap();
        assert_eq!(tail["offset"], 4);
        assert_eq!(tail["next_offset"], 8);
        assert!(tail.get("content").is_none());
        let beyond = fixture
            .call(
                "read_file_chunk",
                json!({"path":"bytes","offset":99,"from_end":true}),
            )
            .unwrap();
        assert_eq!(beyond["next_offset"], 0);
        assert_eq!(beyond["truncated"], false);
        let metadata = fixture.call("file_info", json!({"path":"bytes"})).unwrap();
        assert_eq!(metadata["size_bytes"], bytes.len());
        assert_eq!(metadata["read_only"], true);
        assert_eq!(
            fixture.call("file_info", json!({"path":"."})).unwrap()["type"],
            "directory"
        );
    }

    #[test]
    fn regular_hardlinks_are_readable_and_atomic_replacement_preserves_other_names() {
        let fixture = Fixture::with_write(true);
        let outside = Fixture::new();
        let original = outside.path.join("original");
        fs::write(&original, b"original").unwrap();
        let alias = fixture.path.join("alias");
        fs::hard_link(&original, &alias).unwrap();
        let before = fixture.call("read_file", json!({"path":"alias"})).unwrap();
        assert_eq!(before["content"], "original");
        let info = fixture.call("file_info", json!({"path":"alias"})).unwrap();
        assert_eq!(info["type"], "file");
        fixture
            .call(
                "write_file",
                json!({"path":"alias","content":"replacement","expected_sha256":before["sha256"]}),
            )
            .unwrap();
        assert_eq!(fs::read(&original).unwrap(), b"original");
        assert_eq!(fs::read(&alias).unwrap(), b"replacement");
    }

    #[test]
    fn traversal_symlinks_and_special_files_fail_closed() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fs::write(outside.path.join("secret"), "outside").unwrap();
        symlink(outside.path.join("secret"), fixture.path.join("leaf")).unwrap();
        symlink(&outside.path, fixture.path.join("intermediate")).unwrap();
        for path in [
            "../secret",
            "a/../secret",
            "/etc/passwd",
            "bad\0name",
            "leaf",
            "intermediate/secret",
        ] {
            assert!(
                fixture.call("read_file", json!({"path":path})).is_err(),
                "{path:?}"
            );
        }
        assert!(fixture.call("file_info", json!({"path":"leaf"})).is_err());
        assert!(
            fixture
                .call("list_directory", json!({"path":"intermediate"}))
                .is_err()
        );
        let listed = fixture.call("list_directory", json!({})).unwrap();
        assert_eq!(listed["entries"][0]["type"], "symlink");
        let fifo = std::ffi::CString::new(fixture.path.join("fifo").to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path in this test's owned directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(fixture.call("read_file", json!({"path":"fifo"})).is_err());
        assert!(fixture.call("file_info", json!({"path":"fifo"})).is_err());
    }

    #[test]
    fn replaced_root_is_rejected_and_large_files_are_bounded() {
        let fixture = Fixture::new();
        let large = fs::File::create(fixture.path.join("large")).unwrap();
        large.set_len(1024 * 1024 + 1).unwrap();
        assert!(
            fixture
                .call("read_file", json!({"path":"large"}))
                .unwrap_err()
                .contains("1 MiB")
        );
        let chunk = fixture
            .call("read_file_chunk", json!({"path":"large","length":1}))
            .unwrap();
        assert_eq!(chunk["size_bytes"], 1024 * 1024 + 1);
        large.set_len(64 * 1024 * 1024 + 1).unwrap();
        // The legacy API returns an empty result without reading the file when
        // the tail-relative position is before its beginning, even above 64 MiB.
        let empty = fixture
            .call(
                "read_file_chunk",
                json!({"path":"large","from_end":true,"offset":999999999}),
            )
            .unwrap();
        assert_eq!(empty["next_offset"], 0);
        assert_eq!(empty["size_bytes"], 64 * 1024 * 1024 + 1);
        assert_eq!(empty["truncated"], false);
        assert!(
            fixture
                .call("read_file_chunk", json!({"path":"large","length":1}))
                .is_err()
        );
        // Move the pinned root below another fixture, then replace its name.
        let outside = Fixture::new();
        let relocated = outside.path.join("relocated");
        fs::rename(&fixture.path, &relocated).unwrap();
        fs::create_dir(&fixture.path).unwrap();
        assert!(fixture.call("list_directory", json!({})).is_err());
    }

    #[test]
    fn batch_partial_errors_casefold_and_scalar_sorting() {
        let fixture = Fixture::new();
        for name in ["z", "ä", "a", "\u{10000}", "\u{e000}", "Straße", "ς", "Ꭰ"] {
            fs::write(fixture.path.join(name), "你好\nneedle here\n").unwrap();
        }
        let list = fixture.call("list_directory", json!({})).unwrap();
        let names: Vec<&str> = list["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["Straße", "a", "z", "ä", "ς", "Ꭰ", "\u{e000}", "\u{10000}"]
        );
        for (query, name) in [("STRASSE", "Straße"), ("Σ", "ς"), ("ꭰ", "Ꭰ")] {
            let found = fixture
                .call("search_files", json!({"query":query}))
                .unwrap();
            assert_eq!(found["matches"][0]["path"], name);
        }
        let content = fixture
            .call(
                "search_files",
                json!({"query":"needle","content":true,"limit":1}),
            )
            .unwrap();
        assert_eq!(content["matches"][0]["line"], 2);
        let batch = fixture
            .call(
                "read_multiple_files",
                json!({"paths":["missing","a","z"],"total_limit":3}),
            )
            .unwrap();
        assert!(batch["files"][0]["error"].is_string());
        assert_eq!(batch["files"][1]["content"], "你");
        assert_eq!(batch["files"][2]["error"], "Total response limit exceeded");
        assert_eq!(batch["total_bytes"], 3);
        assert_eq!(batch["truncated"], true);
        assert!(
            fixture
                .call("read_file", json!({"path":"a","offset":1.5}))
                .is_err()
        );
        assert!(
            fixture
                .call("read_file", json!({"path":"a","length":true}))
                .is_err()
        );
        assert!(
            fixture
                .call("read_file", json!({"path":"a","offset":0.0,"length":3.0}))
                .is_ok()
        );
    }

    #[test]
    fn synchronous_search_preview_never_splits_a_surrogate_pair() {
        let fixture = Fixture::new();
        let path = fixture.path.join("preview");
        let first = format!("X{}😀tail", "a".repeat(498));
        fs::write(&path, &first).unwrap();
        let result = fixture
            .call("search_files", json!({"query":"X","content":true}))
            .unwrap();
        let preview = result["matches"][0]["text"].as_str().unwrap();
        assert_eq!(preview, format!("X{}", "a".repeat(498)));

        let second = format!("X{}😀tail", "a".repeat(497));
        fs::write(&path, &second).unwrap();
        let result = fixture
            .call("search_files", json!({"query":"X","content":true}))
            .unwrap();
        let preview = result["matches"][0]["text"].as_str().unwrap();
        assert_eq!(preview, format!("X{}😀", "a".repeat(497)));
    }

    #[test]
    fn read_operations_preserve_legacy_validation_results() {
        let fixture = Fixture::new();
        fs::write(fixture.path.join("file"), "content").unwrap();
        assert_eq!(
            fixture
                .call("read_file", json!({"path":"file","length":0}))
                .unwrap_err(),
            "Invalid byte offset or length (1..65536)"
        );
        assert_eq!(
            fixture
                .call("read_file_chunk", json!({"path":"file","from_end":null}))
                .unwrap_err(),
            "Invalid byte offset or length"
        );
        assert_eq!(
            fixture
                .call("read_file_chunk", json!({"path":"."}))
                .unwrap_err(),
            "A file name is required"
        );
        let batch = fixture
            .call("read_multiple_files", json!({"paths":["file"],"length":0}))
            .unwrap();
        assert_eq!(batch["files"][0]["error"], "Invalid byte offset or length");
        assert_eq!(
            fixture
                .files
                .dispatch("read_file", &json!({"root":null,"path":null}))
                .unwrap_err(),
            "Unknown root; use device_info to discover configured roots"
        );
    }

    #[test]
    fn directory_pagination_accepts_large_integral_offsets() {
        let fixture = Fixture::new();
        fs::write(fixture.path.join("file"), "content").unwrap();
        for offset in [9_007_199_254_740_992.0, 1e100] {
            let page = fixture
                .call("list_directory", json!({"offset":offset}))
                .unwrap();
            assert_eq!(page["entries"], json!([]));
            assert_eq!(page["total"], 1);
            assert_eq!(page["next_offset"], Value::Null);
        }
        assert_eq!(
            fixture
                .call("list_directory", json!({"offset":-1}))
                .unwrap_err(),
            "Invalid offset/limit"
        );
    }

    #[test]
    fn edit_with_non_string_digest_cannot_change_the_file() {
        let fixture = Fixture::with_write(true);
        let path = fixture.path.join("file");
        fs::write(&path, "original").unwrap();
        let error = fixture
            .call(
                "edit_file",
                json!({"path":"file","old_text":"original","new_text":"new","expected_sha256":null}),
            )
            .unwrap_err();
        assert!(error.ends_with("Content conflict: read_file again"));
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
    }
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files")
    )
))]
mod mutation_tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        path: PathBuf,
        files: Files,
    }
    impl Fixture {
        fn new(allow_write: bool) -> Self {
            let time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh-rust-mutation-{}-{time}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            let mut fixture = Self {
                path,
                files: Files {
                    roots: BTreeMap::new(),
                    allow_write,
                    mutation: std::sync::Mutex::new(()),
                },
            };
            fixture.files = Files::new(
                &BTreeMap::from([("work".into(), fixture.path.to_str().unwrap().into())]),
                allow_write,
            )
            .unwrap();
            fixture
        }
        fn call(&self, operation: &str, mut params: Value) -> Result<Value, String> {
            params["root"] = json!("work");
            self.files.dispatch(operation, &params)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    fn writes_create_only_check_digest_preserve_mode_and_cleanup() {
        let fixture = Fixture::new(true);
        let first = fixture
            .call(
                "write_file",
                json!({"path":"nested/file","content":"before"}),
            )
            .unwrap();
        assert_eq!(first["sha256"], digest(b"before"));
        assert_eq!(first["size_bytes"], 6);
        let target = fixture.path.join("nested/file");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(fixture.path.join("nested"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        for expected in ["", "bad"] {
            let error = fixture
                .call(
                    "write_file",
                    json!({"path":"nested/file","content":"bad","expected_sha256":expected}),
                )
                .unwrap_err();
            assert!(error.contains("Content conflict"));
        }
        assert_eq!(fs::read(&target).unwrap(), b"before");
        fixture.call("write_file",json!({"path":"nested/file","content":"after 世界","expected_sha256":first["sha256"]})).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "after 世界");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(
            fs::read_dir(fixture.path.join("nested"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".rdh-"))
        );
        let missing = fixture
            .call(
                "write_file",
                json!({"path":"missing","content":"bad","expected_sha256":"bad"}),
            )
            .unwrap_err();
        assert!(missing.contains("no longer exists"));
        assert!(!fixture.path.join("missing").exists());
        let info = fixture
            .call("file_info", json!({"path":"nested/file"}))
            .unwrap();
        assert_eq!(info["read_only"], false);
        let batch = fixture
            .call("read_multiple_files", json!({"paths":["nested/file"]}))
            .unwrap();
        assert_eq!(batch["files"][0]["read_only"], true);
    }

    #[test]
    fn edits_keep_bom_and_require_exactly_one_nonoverlapping_match() {
        let fixture = Fixture::new(true);
        let bytes = "\u{feff}one 世界 two".as_bytes();
        fs::write(fixture.path.join("bom"), bytes).unwrap();
        let edited = fixture.call("edit_file",json!({"path":"bom","old_text":"世界","new_text":"😀","expected_sha256":digest(bytes)})).unwrap();
        let expected = "\u{feff}one 😀 two".as_bytes();
        assert_eq!(fs::read(fixture.path.join("bom")).unwrap(), expected);
        assert_eq!(edited["sha256"], digest(expected));
        for (old, hash) in [
            ("one", "wrong"),
            ("absent", edited["sha256"].as_str().unwrap()),
            ("", edited["sha256"].as_str().unwrap()),
        ] {
            assert!(
                fixture
                    .call(
                        "edit_file",
                        json!({"path":"bom","old_text":old,"new_text":"bad","expected_sha256":hash})
                    )
                    .is_err()
            );
        }
        fs::write(fixture.path.join("duplicate"), "one one").unwrap();
        let error = fixture.call("edit_file",json!({"path":"duplicate","old_text":"one","new_text":"bad","expected_sha256":digest(b"one one")})).unwrap_err();
        assert!(error.contains("exactly once"));
        fs::write(fixture.path.join("utf16"), [0xff, 0xfe, 0x61, 0]).unwrap();
        assert!(fixture.call("edit_file",json!({"path":"utf16","old_text":"a","new_text":"b","expected_sha256":digest(&[0xff,0xfe,0x61,0])})).unwrap_err().contains("valid UTF-8"));
        fs::write(fixture.path.join("overlap"), "aaa").unwrap();
        fixture.call("edit_file",json!({"path":"overlap","old_text":"aa","new_text":"b","expected_sha256":digest(b"aaa")})).unwrap();
        assert_eq!(fs::read(fixture.path.join("overlap")).unwrap(), b"ba");
    }

    #[test]
    fn mutations_reject_read_only_traversal_symlinks_and_oversize() {
        let readonly = Fixture::new(false);
        for (operation, params) in [
            ("write_file", json!({"path":"file","content":"value"})),
            (
                "edit_file",
                json!({"path":"file","old_text":"a","new_text":"b","expected_sha256":"x"}),
            ),
            ("make_directory", json!({"path":"new"})),
            ("move_file", json!({"source":"a","destination":"b"})),
        ] {
            assert!(
                readonly
                    .call(operation, params)
                    .unwrap_err()
                    .contains("disabled")
            );
        }
        let fixture = Fixture::new(true);
        let outside = Fixture::new(true);
        fs::write(outside.path.join("secret"), "outside").unwrap();
        symlink(&outside.path, fixture.path.join("linked-dir")).unwrap();
        symlink(
            outside.path.join("secret"),
            fixture.path.join("linked-file"),
        )
        .unwrap();
        for path in [
            "../escape",
            "/tmp/escape",
            "linked-dir/secret",
            "linked-file",
        ] {
            assert!(
                fixture
                    .call("write_file", json!({"path":path,"content":"bad"}))
                    .is_err()
            );
        }
        assert!(
            fixture
                .call("make_directory", json!({"path":"linked-dir/child"}))
                .is_err()
        );
        assert!(fixture.call("edit_file",json!({"path":"linked-file","old_text":"outside","new_text":"bad","expected_sha256":digest(b"outside")})).is_err());
        assert_eq!(fs::read(outside.path.join("secret")).unwrap(), b"outside");
        assert!(!outside.path.join("child").exists());
        assert!(
            fixture
                .call(
                    "write_file",
                    json!({"path":"large","content":"x".repeat(1024*1024+1)})
                )
                .is_err()
        );
        assert!(!fixture.path.join("large").exists());
    }

    #[test]
    fn recursive_mkdir_is_idempotent_and_bounded() {
        let fixture = Fixture::new(true);
        for _ in 0..2 {
            let created = fixture
                .call("make_directory", json!({"path":"a/b/c"}))
                .unwrap();
            assert_eq!(created["created"], "a/b/c");
        }
        assert!(fixture.path.join("a/b/c").is_dir());
        assert!(fixture.call("make_directory", json!({"path":"."})).is_ok());
        let too_deep = vec!["d"; 129].join("/");
        assert!(
            fixture
                .call("make_directory", json!({"path":too_deep}))
                .unwrap_err()
                .contains("128")
        );
        assert!(!fixture.path.join("d").exists());
        let boundary = vec!["d"; 128].join("/");
        fixture
            .call("make_directory", json!({"path":boundary}))
            .unwrap();
        assert!(fixture.path.join(&boundary).is_dir());
        fs::write(fixture.path.join("ordinary"), b"file").unwrap();
        assert!(
            fixture
                .call("make_directory", json!({"path":"ordinary/child"}))
                .is_err()
        );
    }

    #[test]
    fn concurrent_updates_and_creates_have_one_winner() {
        let fixture = Fixture::new(true);
        fs::write(fixture.path.join("shared"), b"before").unwrap();
        let expected = digest(b"before");
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                fixture.call(
                    "write_file",
                    json!({"path":"shared","content":"first","expected_sha256":expected}),
                )
            });
            let second = scope.spawn(|| {
                fixture.call(
                    "write_file",
                    json!({"path":"shared","content":"second","expected_sha256":expected}),
                )
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| e.contains("Content conflict"))
        );
        let results = std::thread::scope(|scope| {
            let first =
                scope.spawn(|| fixture.call("write_file", json!({"path":"new","content":"first"})));
            let second = scope
                .spawn(|| fixture.call("write_file", json!({"path":"new","content":"second"})));
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(matches!(
            fs::read_to_string(fixture.path.join("new"))
                .unwrap()
                .as_str(),
            "first" | "second"
        ));
    }

    #[test]
    fn move_accepts_both_formats_and_refuses_overwrite_symlinks_and_cross_root() {
        let fixture = Fixture::new(true);
        fs::write(fixture.path.join("source"), b"move me").unwrap();
        fs::write(fixture.path.join("existing"), b"keep").unwrap();
        fs::create_dir(fixture.path.join("dir")).unwrap();
        let moved = fixture
            .call(
                "move_file",
                json!({"source":"source","destination":"moved"}),
            )
            .unwrap();
        assert_eq!(
            moved,
            json!({"path":"source","destination_path":"moved","moved":true})
        );
        assert!(!fixture.path.join("source").exists());
        assert_eq!(fs::read(fixture.path.join("moved")).unwrap(), b"move me");
        assert!(
            fixture
                .call(
                    "move_file",
                    json!({"source":"moved","destination":"existing"})
                )
                .unwrap_err()
                .contains("exists")
        );
        assert_eq!(fs::read(fixture.path.join("existing")).unwrap(), b"keep");
        assert!(
            fixture
                .call("move_file", json!({"source":"moved","destination":"moved"}))
                .is_err()
        );
        assert!(
            fixture
                .call("move_file", json!({"source":"dir","destination":"renamed"}))
                .is_err()
        );
        assert!(
            fixture
                .call(
                    "move_file",
                    json!({"path":"moved","destination_root":"other","destination_path":"moved"})
                )
                .unwrap_err()
                .contains("across roots")
        );
        assert!(
            fixture
                .call(
                    "move_file",
                    json!({"source":"moved","destination":"next","path":"moved"})
                )
                .unwrap_err()
                .contains("Mixed")
        );
        assert!(
            fixture
                .call(
                    "move_file",
                    json!({"source":"moved","destination":"missing/next"})
                )
                .is_err()
        );
        symlink("moved", fixture.path.join("link")).unwrap();
        assert!(
            fixture
                .call("move_file", json!({"source":"link","destination":"next"}))
                .is_err()
        );
        assert!(
            fixture
                .call("move_file", json!({"source":"moved","destination":"link"}))
                .is_err()
        );
        fixture
            .call(
                "move_file",
                json!({"path":"moved","destination_root":"work","destination_path":"dir/final"}),
            )
            .unwrap();
        assert_eq!(
            fs::read(fixture.path.join("dir/final")).unwrap(),
            b"move me"
        );
    }

    #[test]
    fn concurrent_moves_leave_exactly_one_destination() {
        let fixture = Fixture::new(true);
        fs::write(fixture.path.join("source"), b"one winner").unwrap();
        let results = std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                fixture.call(
                    "move_file",
                    json!({"source":"source","destination":"first"}),
                )
            });
            let second = scope.spawn(|| {
                fixture.call(
                    "move_file",
                    json!({"source":"source","destination":"second"}),
                )
            });
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(!fixture.path.join("source").exists());
        assert_ne!(
            fixture.path.join("first").exists(),
            fixture.path.join("second").exists()
        );
    }

    #[test]
    fn expired_or_closing_queued_mutation_never_starts() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let fixture = Fixture::new(true);
        let closing = AtomicBool::new(false);
        let guard = fixture.files.mutation.lock().unwrap();
        let deadline = Instant::now() + Duration::from_millis(10);
        let (finished, observed) = std::sync::mpsc::sync_channel(1);
        let error = std::thread::scope(|scope| {
            let pending = scope.spawn(|| {
                let result = fixture.files.dispatch_controlled(
                    "write_file",
                    &json!({"root":"work","path":"late","content":"must not be written"}),
                    deadline,
                    &closing,
                );
                finished.send(()).unwrap();
                result
            });
            // Expiration must be observed while the permit is still held.
            // A late-scheduled worker must not receive a fresh deadline.
            let completed = observed.recv_timeout(Duration::from_secs(5));
            drop(guard);
            let result = pending.join().unwrap();
            assert!(
                completed.is_ok(),
                "Queued mutation did not honor its deadline"
            );
            result.unwrap_err()
        });
        assert_eq!(error, MUTATION_DEADLINE_EXPIRED);
        assert!(!fixture.path.join("late").exists());
        closing.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            fixture
                .files
                .dispatch_controlled(
                    "write_file",
                    &json!({"root":"work","path":"late","content":"x"}),
                    Instant::now() + Duration::from_secs(1),
                    &closing,
                )
                .unwrap_err(),
            MUTATION_SHUTTING_DOWN
        );
        assert!(!fixture.path.join("late").exists());
    }
}

impl Files {
    pub fn new(roots: &BTreeMap<String, String>, allow_write: bool) -> Result<Self, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            let mut handles = BTreeMap::new();
            for (name, path) in roots {
                handles.insert(name.clone(), posix::Root::new(path)?);
            }
            Ok(Self {
                roots: handles,
                allow_write,
                mutation: std::sync::Mutex::new(()),
            })
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (roots, allow_write);
            Err("Secure native file operations are unsupported on this platform".into())
        }
    }

    pub fn dispatch(&self, operation: &str, params: &Value) -> Result<Value, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::dispatch(self, operation, params, None)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (operation, params);
            Err("Secure native file operations are unsupported on this platform".into())
        }
    }

    pub fn dispatch_controlled(
        &self,
        operation: &str,
        params: &Value,
        deadline: std::time::Instant,
        closing: &std::sync::atomic::AtomicBool,
    ) -> Result<Value, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::dispatch(self, operation, params, Some((deadline, closing)))
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (operation, params, deadline, closing);
            Err("Secure native file operations are unsupported on this platform".into())
        }
    }

    pub fn working_directory(&self, root: &str, path: &str) -> Result<std::fs::File, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::working_directory(self, root, path)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (root, path);
            Err("Secure native file operations are unsupported on this platform".into())
        }
    }

    // Background searches validate the request before allocating a task, but
    // report failures opening an otherwise valid directory on the task itself.
    pub(crate) fn validate_search_path(&self, root: &str, path: &str) -> Result<(), String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::validate_search_path(self, root, path)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (root, path);
            Err("Secure native search is unsupported on this platform".into())
        }
    }

    pub(crate) fn search_entries(
        &self,
        root: &str,
        path: &str,
        remaining: usize,
        cancelled: &std::sync::atomic::AtomicBool,
        deadline: std::time::Instant,
    ) -> Result<(Vec<SearchEntry>, bool), String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::search_entries(self, root, path, remaining, cancelled, deadline)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (root, path, remaining, cancelled, deadline);
            Err("Secure native search is unsupported on this platform".into())
        }
    }

    pub(crate) fn search_read(&self, root: &str, path: &str) -> Result<Vec<u8>, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::search_read(self, root, path)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            let _ = (root, path);
            Err("Secure native search is unsupported on this platform".into())
        }
    }

    pub(crate) fn search_fold(value: &str) -> String {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        ))]
        {
            posix::casefold(value)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )))]
        {
            value.to_lowercase()
        }
    }

    #[cfg(all(
        test,
        any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files")
        )
    ))]
    pub(crate) fn hold_mutation_for_test(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutation.lock().unwrap()
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files")
))]
mod posix {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        ffi::{CStr, CString},
        fs::{File, Metadata},
        io::{Read, Seek, SeekFrom, Write},
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::fs::MetadataExt,
        },
        time::{Duration, Instant},
    };

    const MAX_FILE: usize = 1024 * 1024;
    const MAX_CHUNK_FILE: u64 = 64 * 1024 * 1024;
    const MAX_ENTRIES: usize = 10000;
    const MAX_SAFE: u64 = 9007199254740991;

    pub(super) struct Root {
        file: File,
        path: String,
    }

    fn io_error(e: std::io::Error) -> String {
        // OS errors contain no requested paths or user file contents.
        format!("File operation failed: {e}")
    }
    fn cstring(s: &str) -> Result<CString, String> {
        CString::new(s).map_err(|_| "Path must contain no NUL bytes".into())
    }

    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    fn open_beneath(parent: &File, path: &str, flags: i32) -> Result<File, String> {
        open_beneath_mode(parent, path, flags, 0)
    }

    #[cfg(target_os = "linux")]
    fn open_beneath_mode(parent: &File, path: &str, flags: i32, mode: u32) -> Result<File, String> {
        let name = cstring(path)?;
        let how = OpenHow {
            flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
            mode: u64::from(mode),
            resolve: 0x08 | 0x04 | 0x02,
        };
        // SAFETY: name/how remain alive through the syscall. Success returns a
        // fresh owned descriptor; no descriptor is constructed on failure.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                parent.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
                return Err("Secure file operations require kernel openat2 support".into());
            }
            return Err(io_error(error));
        }
        // SAFETY: this descriptor was returned uniquely by openat2 above.
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    #[cfg(target_os = "macos")]
    fn open_beneath_mode(parent: &File, path: &str, flags: i32, mode: u32) -> Result<File, String> {
        super::macos::open_beneath(parent, &normalize(path)?, flags, mode).map_err(io_error)
    }

    fn absolute_root(path: &str) -> Result<File, String> {
        if !path.starts_with('/') || path.encode_utf16().count() > 4096 {
            return Err("Root must be an absolute directory path".into());
        }
        #[cfg(target_os = "macos")]
        {
            super::macos::check_capabilities().map_err(io_error)?;
            if std::fs::canonicalize(path).map_err(io_error)? != std::path::Path::new(path) {
                return Err("Root must be a canonical directory".into());
            }
        }
        let slash = cstring("/")?;
        // SAFETY: slash is NUL terminated; open returns a fresh descriptor.
        let fd = unsafe {
            libc::open(
                slash.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        // SAFETY: ownership of fd is transferred exactly once.
        let root = unsafe { File::from_raw_fd(fd) };
        let relative = normalize(path.trim_start_matches('/'))?;
        open_beneath(&root, &relative, libc::O_RDONLY | libc::O_DIRECTORY)
    }

    impl Root {
        pub(super) fn new(path: &str) -> Result<Self, String> {
            Ok(Self {
                file: absolute_root(path)?,
                path: path.into(),
            })
        }
        fn verify(&self) -> Result<(), String> {
            let named = absolute_root(&self.path)?;
            let a = self.file.metadata().map_err(io_error)?;
            let b = named.metadata().map_err(io_error)?;
            if a.dev() != b.dev() || a.ino() != b.ino() {
                return Err("Configured root directory changed".into());
            }
            Ok(())
        }
        fn open(&self, path: &str, directory: bool) -> Result<File, String> {
            self.verify()?;
            open_beneath(
                &self.file,
                &normalize(path)?,
                libc::O_RDONLY | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
            )
        }
        fn metadata(&self, path: &str) -> Result<MetadataFacts, String> {
            self.verify()?;
            let path = normalize(path)?;
            #[cfg(target_os = "linux")]
            {
                // Metadata-only descriptors do not activate special devices.
                let metadata = open_beneath(&self.file, &path, libc::O_PATH)?
                    .metadata()
                    .map_err(io_error)?;
                Ok(MetadataFacts::from_metadata(&metadata))
            }
            #[cfg(target_os = "macos")]
            {
                if path == "." {
                    return Ok(MetadataFacts::from_metadata(
                        &self.file.metadata().map_err(io_error)?,
                    ));
                }
                let (relative, name) = path.rsplit_once('/').unwrap_or((".", &path));
                let parent = Parent::directory(self, relative)?;
                parent.verify()?;
                let stat = super::macos::metadata_at(&parent.directory, name).map_err(io_error)?;
                parent.verify()?;
                // fstatat never opens or follows the leaf, including devices,
                // FIFOs and symlinks. Callers accept only regular files/dirs.
                Ok(MetadataFacts::from_stat(&stat))
            }
        }
    }

    pub(super) fn working_directory(files: &Files, name: &str, path: &str) -> Result<File, String> {
        let root = files.roots.get(name).ok_or_else(|| {
            "Unknown root; use device_info to discover configured roots".to_owned()
        })?;
        let directory = root.open(path, true)?;
        root.verify()?;
        Ok(directory)
    }

    pub(super) fn validate_search_path(
        files: &Files,
        name: &str,
        path: &str,
    ) -> Result<(), String> {
        if !files.roots.contains_key(name) {
            return Err("Unknown root; use device_info to discover configured roots".into());
        }
        normalize(path).map(|_| ())
    }

    fn normalize(path: &str) -> Result<String, String> {
        if path.starts_with('/') || path.contains('\0') || path.encode_utf16().count() > 4096 {
            return Err("Path must be relative and contain no NUL bytes".into());
        }
        let mut parts = Vec::new();
        for part in path.split('/') {
            if part == ".." {
                return Err("Path must be relative to a root, with no '..' or NUL".into());
            }
            if !part.is_empty() && part != "." {
                parts.push(part);
            }
        }
        Ok(if parts.is_empty() {
            ".".into()
        } else {
            parts.join("/")
        })
    }
    fn string<'a>(p: &'a Value, name: &str, default: Option<&'a str>) -> Result<&'a str, String> {
        match p.get(name) {
            None => default.ok_or_else(|| format!("Missing {name}")),
            Some(v) => v.as_str().ok_or_else(|| format!("Invalid {name}")),
        }
    }
    fn number(p: &Value, name: &str, default: u64, min: u64, max: u64) -> Result<u64, String> {
        let Some(value) = p.get(name) else {
            return Ok(default);
        };
        // Protocol v1 accepts integral JSON numbers such as 1.0, but not bools.
        let n = value.as_f64().ok_or_else(|| format!("Invalid {name}"))?;
        if !n.is_finite() || n.fract() != 0.0 || n < min as f64 || n > max as f64 {
            return Err(format!("Invalid {name}"));
        }
        Ok(n as u64)
    }
    fn boolean(p: &Value, name: &str, default: bool) -> Result<bool, String> {
        match p.get(name) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| format!("Invalid {name}")),
        }
    }
    fn root<'a>(files: &'a Files, params: &Value) -> Result<&'a Root, String> {
        params
            .get("root")
            .and_then(Value::as_str)
            .and_then(|name| files.roots.get(name))
            .ok_or_else(|| "Unknown root; use device_info to discover configured roots".into())
    }

    fn file_path(params: &Value) -> Result<&str, String> {
        params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "Path must be relative and contain no NUL bytes".into())
    }
    fn regular(file: &File, cap: u64) -> Result<Metadata, String> {
        let m = file.metadata().map_err(io_error)?;
        if !m.is_file() {
            return Err("Only regular files are supported".into());
        }
        if m.len() > cap {
            return Err(if cap == MAX_FILE as u64 {
                "File exceeds 1 MiB"
            } else {
                "File exceeds 64 MiB"
            }
            .into());
        }
        Ok(m)
    }
    fn read_full(file: &mut File) -> Result<Vec<u8>, String> {
        regular(file, MAX_FILE as u64)?;
        let mut bytes = Vec::new();
        file.take((MAX_FILE + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > MAX_FILE {
            return Err("File exceeds 1 MiB".into());
        }
        Ok(bytes)
    }
    fn mtime(m: &Metadata) -> f64 {
        m.mtime() as f64 * 1000.0 + m.mtime_nsec() as f64 / 1_000_000.0
    }

    // The old Node Agent embeds Number.toString() values in fingerprints.
    // serde_json supplies the shortest rounded decimal digits; arrange those
    // digits using ECMAScript's fixed/scientific notation thresholds. Metadata
    // values are finite, including after converting the integer fields to f64.
    // https://tc39.es/ecma262/2024/#sec-numeric-types-number-tostring
    pub(super) fn js_number(value: f64) -> String {
        if value == 0.0 {
            return "0".into();
        }
        let serialized = serde_json::to_string(&value).expect("finite metadata number");
        let (sign, number) = serialized
            .strip_prefix('-')
            .map_or(("", serialized.as_str()), |number| ("-", number));
        let (mantissa, exponent) = number.split_once('e').map_or((number, 0), |(part, exp)| {
            (part, exp.parse::<i32>().expect("JSON exponent"))
        });
        let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
        let all_digits = mantissa.replace('.', "");
        let first = all_digits
            .find(|digit| digit != '0')
            .expect("nonzero number");
        let position = decimal + exponent - first as i32;
        let digits = all_digits[first..].trim_end_matches('0');
        let length = digits.len() as i32;

        if length <= position && position <= 21 {
            format!("{sign}{digits}{}", "0".repeat((position - length) as usize))
        } else if position > 0 && position <= 21 {
            format!(
                "{sign}{}.{}",
                &digits[..position as usize],
                &digits[position as usize..]
            )
        } else if position > -6 && position <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-position) as usize))
        } else {
            let exp = position - 1;
            let fraction = if digits.len() > 1 {
                format!(".{}", &digits[1..])
            } else {
                String::new()
            };
            let plus = if exp >= 0 { "+" } else { "" };
            format!("{sign}{}{fraction}e{plus}{exp}", &digits[..1])
        }
    }

    pub(super) fn js_unsigned(value: u64) -> String {
        if value <= 9_007_199_254_740_992 {
            value.to_string()
        } else {
            js_number(value as f64)
        }
    }

    fn fingerprint_parts(dev: u64, ino: u64, length: u64, modified_ms: f64) -> String {
        format!(
            "{}:{}:{}:{}",
            js_unsigned(dev),
            js_unsigned(ino),
            js_unsigned(length),
            js_number(modified_ms)
        )
    }
    fn fingerprint(m: &Metadata) -> String {
        fingerprint_parts(m.dev(), m.ino(), m.len(), mtime(m))
    }
    struct MetadataFacts {
        dev: u64,
        ino: u64,
        size: u64,
        mode: u32,
        modified_ms: f64,
        is_file: bool,
        is_directory: bool,
    }
    impl MetadataFacts {
        fn from_metadata(m: &Metadata) -> Self {
            Self {
                dev: m.dev(),
                ino: m.ino(),
                size: m.len(),
                mode: m.mode(),
                modified_ms: mtime(m),
                is_file: m.is_file(),
                is_directory: m.is_dir(),
            }
        }
        #[cfg(target_os = "macos")]
        fn from_stat(m: &libc::stat) -> Self {
            let mode = u32::from(m.st_mode);
            Self {
                dev: m.st_dev as u64,
                ino: m.st_ino,
                size: m.st_size.max(0) as u64,
                mode,
                modified_ms: m.st_mtime as f64 * 1000.0 + m.st_mtime_nsec as f64 / 1_000_000.0,
                is_file: m.st_mode & libc::S_IFMT == libc::S_IFREG,
                is_directory: m.st_mode & libc::S_IFMT == libc::S_IFDIR,
            }
        }
        fn fingerprint(&self) -> String {
            fingerprint_parts(self.dev, self.ino, self.size, self.modified_ms)
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Encoding {
        Utf8,
        Le,
        Be,
    }
    impl Encoding {
        fn name(self) -> &'static str {
            match self {
                Self::Utf8 => "utf-8",
                Self::Le => "utf-16le",
                Self::Be => "utf-16be",
            }
        }
        fn decode(self, b: &[u8]) -> Result<String, String> {
            let invalid = || {
                format!(
                    "File is not valid {}; use read_file_chunk for exact bytes",
                    self.name().to_uppercase()
                )
            };
            if self == Self::Utf8 {
                return std::str::from_utf8(b)
                    .map(str::to_owned)
                    .map_err(|_| invalid());
            }
            if b.len() % 2 != 0 {
                return Err(invalid());
            }
            let words: Vec<u16> = b
                .chunks_exact(2)
                .map(|x| {
                    if self == Self::Le {
                        u16::from_le_bytes([x[0], x[1]])
                    } else {
                        u16::from_be_bytes([x[0], x[1]])
                    }
                })
                .collect();
            String::from_utf16(&words).map_err(|_| invalid())
        }
    }
    struct TextRange {
        text: String,
        encoding: Encoding,
        start: usize,
        end: usize,
    }
    fn text_range(bytes: &[u8], offset: u64, length: usize) -> Result<TextRange, String> {
        let encoding = if bytes.starts_with(&[0xff, 0xfe]) {
            Encoding::Le
        } else if bytes.starts_with(&[0xfe, 0xff]) {
            Encoding::Be
        } else {
            Encoding::Utf8
        };
        encoding.decode(bytes)?; // Validate the whole file, preserving any BOM.
        let start = offset.min(bytes.len() as u64) as usize;
        if encoding != Encoding::Utf8 && start % 2 != 0 {
            return Err("offset must be aligned to a UTF-16 code unit".into());
        }
        let requested_end = (start + length).min(bytes.len());
        for trim in 0..=3 {
            let Some(end) = requested_end.checked_sub(trim).filter(|end| *end >= start) else {
                break;
            };
            if let Ok(text) = encoding.decode(&bytes[start..end]) {
                if end == start && start < bytes.len() {
                    break;
                }
                return Ok(TextRange {
                    text,
                    encoding,
                    start,
                    end,
                });
            }
        }
        Err(format!(
            "Byte range cannot fit complete {} characters; align offset or increase length",
            encoding.name().to_uppercase()
        ))
    }
    fn text_value(path: &str, bytes: &[u8], r: &TextRange) -> Value {
        json!({"path":path,"content":r.text,"content_base64":STANDARD.encode(&bytes[r.start..r.end]),
            "content_encoding":r.encoding.name(),"utf8_valid":r.encoding == Encoding::Utf8,
            "offset":r.start,"next_offset":r.end,"size_bytes":bytes.len(),"truncated":r.end < bytes.len()})
    }
    fn read(files: &Files, p: &Value) -> Result<Value, String> {
        let invalid_range = |_| "Invalid byte offset or length (1..65536)".to_string();
        let offset = number(p, "offset", 0, 0, MAX_SAFE).map_err(invalid_range)?;
        let length = number(p, "length", 32768, 1, 65536).map_err(invalid_range)?;
        let root = root(files, p)?;
        let path = file_path(p)?;
        if normalize(path)? == "." {
            return Err("A file name is required".into());
        }
        let bytes = read_full(&mut root.open(path, false)?)?;
        root.verify()?;
        let range = text_range(&bytes, offset, length as usize)?;
        let mut result = text_value(path, &bytes, &range);
        result["sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
        Ok(result)
    }
    fn info(files: &Files, p: &Value) -> Result<Value, String> {
        let root = root(files, p)?;
        let path = file_path(p)?;
        let m = root.metadata(path)?;
        if !m.is_file && !m.is_directory {
            return Err("Only regular files and directories are supported".into());
        }
        root.verify()?;
        Ok(
            json!({"path":path,"type":if m.is_file {"file"} else {"directory"},
            "size_bytes":m.size,"mode":m.mode & 0o777,"mtime_ms":m.modified_ms,
            "read_only":!files.allow_write,"fingerprint":m.fingerprint()}),
        )
    }
    fn chunk(files: &Files, p: &Value) -> Result<Value, String> {
        let encoding = if p.get("encoding").is_none_or(Value::is_null) {
            "both"
        } else {
            string(p, "encoding", None).map_err(|_| "Invalid chunk encoding")?
        };
        if !matches!(encoding, "both" | "text" | "base64") {
            return Err("Invalid chunk encoding".into());
        }
        let invalid_range = |_| "Invalid byte offset or length".to_string();
        let offset = number(p, "offset", 0, 0, MAX_SAFE).map_err(invalid_range)?;
        let length = number(p, "length", 32768, 1, 65536).map_err(invalid_range)?;
        let from_end = boolean(p, "from_end", false).map_err(invalid_range)?;
        let root = root(files, p)?;
        let path = file_path(p)?;
        if normalize(path)? == "." {
            return Err("A file name is required".into());
        }
        let metadata = root.metadata(path)?;
        if !metadata.is_file {
            return Err("Only regular files are supported".into());
        }
        let end = if from_end {
            metadata.size.saturating_sub(offset)
        } else {
            metadata.size
        };
        let start = if from_end {
            end.saturating_sub(length)
        } else {
            offset
        };
        let available = end.saturating_sub(start).min(length);
        if from_end && available == 0 {
            root.verify()?;
            let mut result = json!({"path":path,"utf8_valid":true,"content_encoding":"utf-8",
                "offset":start,"next_offset":start,"size_bytes":metadata.size,"truncated":false,
                "read_only":true,"fingerprint":metadata.fingerprint()});
            if encoding != "base64" {
                result["content"] = json!("");
            }
            if encoding != "text" {
                result["content_base64"] = json!("");
            }
            return Ok(result);
        }
        let mut file = root.open(path, false)?;
        let m = regular(&file, MAX_CHUNK_FILE)?;
        if metadata.dev != m.dev() || metadata.ino != m.ino() {
            return Err("File changed while opening".into());
        }
        file.seek(SeekFrom::Start(start)).map_err(io_error)?;
        let mut bytes = Vec::new();
        file.take(available)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        root.verify()?;
        let valid = std::str::from_utf8(&bytes).is_ok();
        let mut result = json!({"path":path,"utf8_valid":valid,"content_encoding":if valid {"utf-8"} else {"utf-8-replacement"},
            "offset":start,"next_offset":start + bytes.len() as u64,"size_bytes":m.len(),
            "truncated": !(from_end && available == 0) && start + (bytes.len() as u64) < m.len(),
            "read_only":true,"fingerprint":fingerprint(&m)});
        if encoding != "base64" {
            result["content"] = json!(String::from_utf8_lossy(&bytes));
        }
        if encoding != "text" {
            result["content_base64"] = json!(STANDARD.encode(&bytes));
        }
        Ok(result)
    }

    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            // SAFETY: fdopendir allocated this DIR and this owner closes it once.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    struct Entry {
        name: String,
        kind: &'static str,
        size: u64,
    }
    fn entries(file: File) -> Result<Vec<Entry>, String> {
        let (entries, truncated) = entries_bounded(file, MAX_ENTRIES, None)?;
        if truncated {
            Err("Directory exceeds 10000 entries".into())
        } else {
            Ok(entries)
        }
    }

    fn entries_bounded(
        file: File,
        limit: usize,
        control: Option<(&std::sync::atomic::AtomicBool, Instant)>,
    ) -> Result<(Vec<Entry>, bool), String> {
        let fd = file.into_raw_fd();
        // SAFETY: fd is an owned directory descriptor, transferred on success.
        let pointer = unsafe { libc::fdopendir(fd) };
        if pointer.is_null() {
            let error = std::io::Error::last_os_error();
            // SAFETY: fdopendir failed, leaving fd owned by this function.
            unsafe {
                libc::close(fd);
            }
            return Err(io_error(error));
        }
        let dir = Dir(pointer);
        let mut result = Vec::new();
        loop {
            if control.is_some_and(|(cancelled, deadline)| {
                cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline
            }) {
                return Err("Search cancelled or expired".into());
            }
            // SAFETY: errno is thread-local, dir is live and used by one caller.
            unsafe {
                #[cfg(target_os = "linux")]
                {
                    *libc::__errno_location() = 0;
                }
                #[cfg(target_os = "macos")]
                {
                    *libc::__error() = 0;
                }
            }
            let entry = unsafe { libc::readdir(dir.0) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(0) {
                    return Err(io_error(error));
                }
                break;
            }
            // SAFETY: readdir provides a NUL-terminated d_name valid until the
            // next call; name is copied before that call.
            let raw = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let name = raw.to_str().map_err(|_| {
                "Directory contains a filename not representable as UTF-8".to_string()
            })?;
            if name == "." || name == ".." {
                continue;
            }
            if result.len() >= limit {
                return Ok((result, true));
            }
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: raw is valid and stat points to sufficient uninitialized
            // storage. AT_SYMLINK_NOFOLLOW never dereferences a symlink entry.
            if unsafe {
                libc::fstatat(
                    fd,
                    raw.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            // SAFETY: successful fstatat initialized stat.
            let stat = unsafe { stat.assume_init() };
            let kind = match stat.st_mode & libc::S_IFMT {
                libc::S_IFREG => "file",
                libc::S_IFDIR => "directory",
                libc::S_IFLNK => "symlink",
                _ => "other",
            };
            result.push(Entry {
                name: name.into(),
                kind,
                size: stat.st_size.max(0) as u64,
            });
        }
        Ok((result, false))
    }

    pub(super) fn search_entries(
        files: &Files,
        name: &str,
        path: &str,
        remaining: usize,
        cancelled: &std::sync::atomic::AtomicBool,
        deadline: Instant,
    ) -> Result<(Vec<SearchEntry>, bool), String> {
        let root = files.roots.get(name).ok_or("Unknown root")?;
        let (mut entries, truncated) = entries_bounded(
            root.open(path, true)?,
            remaining,
            Some((cancelled, deadline)),
        )?;
        root.verify()?;
        entries.sort_by(|a, b| a.name.encode_utf16().cmp(b.name.encode_utf16()));
        Ok((
            entries
                .into_iter()
                .map(|entry| SearchEntry {
                    name: entry.name,
                    kind: entry.kind,
                })
                .collect(),
            truncated,
        ))
    }

    pub(super) fn search_read(files: &Files, name: &str, path: &str) -> Result<Vec<u8>, String> {
        let root = files.roots.get(name).ok_or("Unknown root")?;
        let result = read_full(&mut root.open(path, false)?)?;
        root.verify()?;
        Ok(result)
    }
    fn list(files: &Files, p: &Value) -> Result<Value, String> {
        let invalid_page = || "Invalid offset/limit".to_string();
        let offset = match p.get("offset") {
            None => 0.0,
            Some(value) => value.as_f64().ok_or_else(invalid_page)?,
        };
        if !offset.is_finite() || offset.fract() != 0.0 || offset < 0.0 {
            return Err(invalid_page());
        }
        let limit = number(p, "limit", 100, 1, 1000).map_err(|_| invalid_page())? as usize;
        let root = root(files, p)?;
        let path = match p.get("path") {
            None => ".",
            Some(value) => value
                .as_str()
                .ok_or("Path must be relative and contain no NUL bytes")?,
        };
        let mut entries = entries(root.open(path, true)?)?;
        root.verify()?;
        // UTF-8 lexical order agrees with Unicode scalar-value order.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let total = entries.len();
        // The legacy API accepts any nonnegative integral JS number. An
        // offset beyond this bounded directory is equivalent to its end.
        let offset = if offset >= total as f64 {
            total
        } else {
            offset as usize
        };
        let selected: Vec<Value> = entries
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|e| json!({"name":e.name,"type":e.kind,"size_bytes":e.size}))
            .collect();
        Ok(
            json!({"entries":selected,"total":total,"next_offset":if offset + limit < total { Some(offset + limit) } else { None }}),
        )
    }

    fn multiple(files: &Files, p: &Value) -> Result<Value, String> {
        let canonical = p.get("paths").and_then(Value::as_array);
        let requests = if let Some(paths) = canonical {
            paths
                .iter()
                .map(|path| {
                    let mut request = json!({"root":p.get("root"),"path":path});
                    for field in ["offset", "length"] {
                        if let Some(v) = p.get(field) {
                            request[field] = v.clone();
                        }
                    }
                    request
                })
                .collect::<Vec<_>>()
        } else {
            p.get("files")
                .and_then(Value::as_array)
                .ok_or("Invalid files/total_limit")?
                .clone()
        };
        let budget = if p.get("total_limit").is_some_and(Value::is_null) {
            256 * 1024
        } else {
            number(p, "total_limit", 256 * 1024, 1, 256 * 1024)? as usize
        };
        if requests.is_empty() || requests.len() > 16 {
            return Err("Invalid files/total_limit".into());
        }
        let mut used = 0;
        let mut truncated = false;
        let mut results = Vec::new();
        for mut request in requests {
            // The batch API uses nullish defaults, unlike read_file's strict
            // offset/length validation. Preserve this distinction.
            for name in ["offset", "length"] {
                if request.get(name).is_some_and(Value::is_null) {
                    if let Some(object) = request.as_object_mut() {
                        object.remove(name);
                    }
                }
            }
            let outcome = (|| -> Result<Value, String> {
                if !request.is_object()
                    || !request["root"].is_string()
                    || !request["path"].is_string()
                {
                    return Err("Invalid file request".into());
                }
                let remaining = budget - used;
                if remaining == 0 {
                    truncated = true;
                    return Err("Total response limit exceeded".into());
                }
                let invalid_range = |_| "Invalid byte offset or length".to_string();
                let length = number(
                    &request,
                    "length",
                    if canonical.is_some() { 16384 } else { 32768 },
                    1,
                    if canonical.is_some() { 16384 } else { 65536 },
                )
                .map_err(invalid_range)?;
                let offset = number(&request, "offset", 0, 0, MAX_SAFE).map_err(invalid_range)?;
                let path = string(&request, "path", None)?;
                let root = root(files, &request)?;
                if normalize(path)? == "." {
                    return Err("A file name is required".into());
                }
                let mut file = root.open(path, false)?;
                let metadata = regular(&file, MAX_FILE as u64)?;
                let bytes = read_full(&mut file)?;
                root.verify()?;
                let mut range = text_range(&bytes, offset, length as usize)?;
                if range.text.len() > remaining {
                    let mut end = 0;
                    for (index, c) in range.text.char_indices() {
                        if index + c.len_utf8() > remaining {
                            break;
                        }
                        end = index + c.len_utf8();
                    }
                    truncated = true;
                    if end == 0 {
                        return Err("Output budget cannot fit the next UTF-8 character".into());
                    }
                    range.text.truncate(end);
                    let source_bytes = if range.encoding == Encoding::Utf8 {
                        end
                    } else {
                        range.text.encode_utf16().count() * 2
                    };
                    range.end = range.start + source_bytes;
                }
                used += range.text.len();
                truncated |= range.end < bytes.len();
                let mut result = text_value(path, &bytes, &range);
                result["read_only"] = json!(true);
                result["fingerprint"] = json!(fingerprint(&metadata));
                Ok(result)
            })();
            results.push(match outcome {
                Ok(value) => value,
                Err(error) => {
                    let mut error_value = json!({"error":error});
                    if let Some(path) = request.get("path") {
                        error_value["path"] = path.clone();
                    }
                    error_value
                }
            });
        }
        Ok(json!({"files":results,"total_bytes":used,"truncated":truncated}))
    }

    struct Search<'a> {
        root: &'a Root,
        query: &'a str,
        folded: String,
        content: bool,
        limit: usize,
        deadline: Instant,
        visited: usize,
        truncated: bool,
        matches: Vec<Value>,
    }
    impl Search<'_> {
        fn walk(&mut self, path: &str, depth: usize) {
            if depth > 30 {
                self.truncated = true;
                return;
            }
            let Ok(entries) = self.root.open(path, true).and_then(entries) else {
                return;
            };
            for entry in entries {
                self.visited += 1;
                if self.visited > MAX_ENTRIES
                    || Instant::now() > self.deadline
                    || self.matches.len() >= self.limit
                {
                    self.truncated = true;
                    return;
                }
                if entry.kind == "symlink" {
                    continue;
                }
                let relative = if path == "." {
                    entry.name.clone()
                } else {
                    format!("{path}/{}", entry.name)
                };
                if !self.content && casefold(&entry.name).contains(&self.folded) {
                    self.matches.push(json!({"path":relative}));
                }
                if entry.kind == "directory" {
                    self.walk(&relative, depth + 1);
                } else if self.content && entry.kind == "file" {
                    let Ok(bytes) = self
                        .root
                        .open(&relative, false)
                        .and_then(|mut f| read_full(&mut f))
                    else {
                        continue;
                    };
                    let Ok(text) = std::str::from_utf8(&bytes) else {
                        continue;
                    };
                    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
                    let mut lines = text.split('\n').enumerate().peekable();
                    while let Some((index, line)) = lines.next() {
                        if Instant::now() > self.deadline {
                            self.truncated = true;
                            return;
                        }
                        let line = if lines.peek().is_some() {
                            line.strip_suffix('\r').unwrap_or(line)
                        } else {
                            line
                        };
                        if line.contains(self.query) {
                            // JS slices UTF-16 code units. Avoid generating an
                            // unpaired surrogate at the 500-unit boundary.
                            let mut units = 0;
                            let preview: String = line
                                .chars()
                                .take_while(|c| {
                                    units += c.len_utf16();
                                    units <= 500
                                })
                                .collect();
                            self.matches
                                .push(json!({"path":relative,"line":index+1,"text":preview}));
                            if self.matches.len() >= self.limit {
                                self.truncated = true;
                                return;
                            }
                        }
                    }
                }
                if self.visited > MAX_ENTRIES || Instant::now() > self.deadline {
                    self.truncated = true;
                    return;
                }
            }
        }
    }
    fn search(files: &Files, p: &Value) -> Result<Value, String> {
        let invalid = || "Query must be 1..512 characters; limit 1..200".to_string();
        let query = p
            .get("query")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.encode_utf16().count() <= 512)
            .ok_or_else(invalid)?;
        let limit = number(p, "limit", 100, 1, 200).map_err(|_| invalid())? as usize;
        let content = boolean(p, "content", false)?;
        let root = root(files, p)?;
        let path = match p.get("path") {
            None => ".",
            Some(value) => value
                .as_str()
                .ok_or("Path must be relative and contain no NUL bytes")?,
        };
        drop(root.open(path, true)?);
        let mut search = Search {
            root,
            query,
            folded: casefold(query),
            content,
            limit,
            deadline: Instant::now() + Duration::from_secs(3),
            visited: 0,
            truncated: false,
            matches: Vec::new(),
        };
        search.walk(path, 0);
        root.verify()?;
        search.matches.truncate(limit);
        Ok(json!({"matches":search.matches,"visited":search.visited,"truncated":search.truncated}))
    }
    /// A directory capability plus its rooted name. Verification detects stale
    /// names before and after mutation. It cannot make the check and renameat /
    /// mkdirat commit one atomic operation against external directory renames.
    struct Parent<'a> {
        root: &'a Root,
        relative: String,
        directory: File,
    }
    impl<'a> Parent<'a> {
        fn directory(root: &'a Root, relative: &str) -> Result<Self, String> {
            let relative = normalize(relative)?;
            let directory = root.open(&relative, true)?;
            Ok(Self {
                root,
                relative,
                directory,
            })
        }
        fn file(root: &'a Root, path: &str, create: bool) -> Result<(Self, String), String> {
            let path = normalize(path)?;
            if path == "." {
                return Err("A file name is required".into());
            }
            let (relative, name) = path.rsplit_once('/').unwrap_or((".", &path));
            if create {
                make_directories(root, relative)?;
            }
            Ok((Self::directory(root, relative)?, name.into()))
        }
        fn verify(&self) -> Result<(), String> {
            let named = self
                .root
                .open(&self.relative, true)?
                .metadata()
                .map_err(io_error)?;
            let held = self.directory.metadata().map_err(io_error)?;
            if held.dev() != named.dev() || held.ino() != named.ino() {
                return Err("Directory path changed while performing file operation".into());
            }
            Ok(())
        }
    }

    fn stat_at(directory: &File, name: &str) -> Result<Option<libc::stat>, String> {
        let name = cstring(name)?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: directory is live, name is NUL terminated and stat has the
        // required size. NOFOLLOW observes symlinks without dereferencing them.
        let result = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(io_error(error));
        }
        // SAFETY: successful fstatat initialized every returned stat field.
        Ok(Some(unsafe { stat.assume_init() }))
    }
    fn same_inode(stat: &libc::stat, metadata: &Metadata) -> bool {
        stat.st_dev == metadata.dev() as libc::dev_t && stat.st_ino == metadata.ino() as libc::ino_t
    }
    fn same_version(stat: &libc::stat, metadata: &Metadata) -> bool {
        same_inode(stat, metadata)
            && stat.st_size >= 0
            && stat.st_size as u64 == metadata.len()
            && stat.st_mode == metadata.mode() as libc::mode_t
            && stat.st_mtime == metadata.mtime()
            && stat.st_mtime_nsec == metadata.mtime_nsec()
            && stat.st_ctime == metadata.ctime()
            && stat.st_ctime_nsec == metadata.ctime_nsec()
    }
    fn verify_file(parent: &Parent<'_>, name: &str, metadata: &Metadata) -> Result<(), String> {
        parent.verify()?;
        let named = stat_at(&parent.directory, name)?
            .ok_or("File changed while performing file operation")?;
        if !same_version(&named, metadata) {
            return Err("File changed while performing file operation".into());
        }
        Ok(())
    }
    fn make_directories(root: &Root, path: &str) -> Result<(), String> {
        let path = normalize(path)?;
        let components: Vec<&str> = if path == "." {
            Vec::new()
        } else {
            path.split('/').collect()
        };
        if components.len() > 128 {
            return Err("Directory creation exceeds 128 path components".into());
        }
        let mut prefix = String::from(".");
        for component in components {
            let parent = Parent::directory(root, &prefix)?;
            parent.verify()?;
            if stat_at(&parent.directory, component)?.is_none() {
                let name = cstring(component)?;
                // SAFETY: this is a single validated component beneath a live
                // directory descriptor. mkdirat does not follow the leaf.
                if unsafe { libc::mkdirat(parent.directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0
                {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(io_error(error));
                    }
                }
                crate::storage::sync_directory(&parent.directory).map_err(io_error)?;
            }
            parent.verify()?;
            prefix = if prefix == "." {
                component.into()
            } else {
                format!("{prefix}/{component}")
            };
            // Validate existing and concurrently created entries through the
            // root. A symlink or non-directory always fails closed here.
            drop(root.open(&prefix, true)?);
        }
        root.verify()
    }

    struct Existing {
        bytes: Vec<u8>,
        metadata: Metadata,
    }
    fn read_existing(parent: &Parent<'_>, name: &str) -> Result<Option<Existing>, String> {
        parent.verify()?;
        let Some(named) = stat_at(&parent.directory, name)? else {
            return Ok(None);
        };
        if named.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err("Only regular files are supported".into());
        }
        let mut file = open_beneath(&parent.directory, name, libc::O_RDONLY | libc::O_NONBLOCK)?;
        let metadata = regular(&file, MAX_FILE as u64)?;
        if !same_version(&named, &metadata) {
            return Err("File changed while opening".into());
        }
        let bytes = read_full(&mut file)?;
        verify_file(parent, name, &metadata)?;
        Ok(Some(Existing { bytes, metadata }))
    }

    fn random_temp_name() -> Result<String, String> {
        crate::random::hex_token::<16>()
            .map(|suffix| format!(".rdh-{suffix}"))
            .map_err(|_| "Secure randomness is unavailable".into())
    }

    struct Temporary<'a> {
        directory: &'a File,
        name: String,
        file: File,
        present: bool,
    }
    impl<'a> Temporary<'a> {
        fn create(directory: &'a File) -> Result<Self, String> {
            let name = random_temp_name()?;
            let file = open_beneath_mode(
                directory,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            Ok(Self {
                directory,
                name,
                file,
                present: true,
            })
        }
        fn verify(&self) -> Result<(), String> {
            let named = stat_at(self.directory, &self.name)?.ok_or("Temporary file disappeared")?;
            let held = self.file.metadata().map_err(io_error)?;
            if named.st_mode & libc::S_IFMT != libc::S_IFREG || !same_version(&named, &held) {
                return Err("Temporary file changed".into());
            }
            Ok(())
        }
        fn remove(&mut self) -> Result<(), String> {
            if !self.present {
                return Ok(());
            }
            self.verify()?;
            let name = cstring(&self.name)?;
            // SAFETY: name is a single component; the owned directory fd is
            // live. unlinkat removes this name and never follows a symlink.
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            self.present = false;
            Ok(())
        }
    }
    impl Drop for Temporary<'_> {
        fn drop(&mut self) {
            // Errors on the normal path are returned explicitly. This is only
            // best-effort cleanup while propagating an already-uncertain error.
            let _ = self.remove();
        }
    }

    fn write_atomic(
        parent: &Parent<'_>,
        name: &str,
        bytes: &[u8],
        existing: Option<&Existing>,
    ) -> Result<(), String> {
        parent.verify()?;
        let mut temporary = Temporary::create(&parent.directory)?;
        let result = (|| -> Result<(), String> {
            // The temporary starts private. Only fully written content receives
            // the target's permissions, before sync and the atomic commit.
            temporary.file.write_all(bytes).map_err(io_error)?;
            let mode = existing.map_or(0o600, |previous| previous.metadata.mode() & 0o777);
            // SAFETY: fchmod operates on our live, exclusively created file.
            if unsafe { libc::fchmod(temporary.file.as_raw_fd(), mode as libc::mode_t) } < 0 {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            temporary.file.sync_all().map_err(io_error)?;
            parent.verify()?;
            temporary.verify()?;
            if let Some(previous) = existing {
                // This detects observed external changes; it is not an atomic
                // cross-process compare-and-swap against the final rename.
                verify_file(parent, name, &previous.metadata)?;
            } else if stat_at(&parent.directory, name)?.is_some() {
                return Err("File already exists".into());
            }
            let source = cstring(&temporary.name)?;
            let target = cstring(name)?;
            if existing.is_some() {
                // SAFETY: both names are single components in the held parent.
                // renameat atomically replaces the name without following it.
                if unsafe {
                    libc::renameat(
                        parent.directory.as_raw_fd(),
                        source.as_ptr(),
                        parent.directory.as_raw_fd(),
                        target.as_ptr(),
                    )
                } < 0
                {
                    return Err(io_error(std::io::Error::last_os_error()));
                }
                temporary.present = false;
            } else {
                // SAFETY: linkat with no FOLLOW flag links the temporary name,
                // and fails with EEXIST instead of overwriting the destination.
                if unsafe {
                    libc::linkat(
                        parent.directory.as_raw_fd(),
                        source.as_ptr(),
                        parent.directory.as_raw_fd(),
                        target.as_ptr(),
                        0,
                    )
                } < 0
                {
                    return Err(io_error(std::io::Error::last_os_error()));
                }
                temporary.remove()?;
            }
            crate::storage::sync_directory(&parent.directory).map_err(io_error)?;
            parent.verify()?;
            let committed =
                stat_at(&parent.directory, name)?.ok_or("Committed file disappeared")?;
            if !same_inode(&committed, &temporary.file.metadata().map_err(io_error)?) {
                return Err("Committed file changed".into());
            }
            Ok(())
        })();
        let cleanup = temporary.remove();
        match (result, cleanup) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn mutation_result(result: Result<Value, String>) -> Result<Value, String> {
        // Includes partial recursive directory creation, commit followed by a
        // failed fsync/identity check, and failed temporary cleanup. RPC must not
        // report not_sent/no-effect for errors returned from this phase.
        result.map_err(|error| {
            format!("File mutation failed; filesystem effects may have occurred: {error}")
        })
    }
    type Control<'a> = Option<(Instant, &'a std::sync::atomic::AtomicBool)>;

    fn lock_mutation<'a>(
        files: &'a Files,
        control: Control<'_>,
    ) -> Result<std::sync::MutexGuard<'a, ()>, String> {
        if !files.allow_write {
            return Err("File writes are disabled in this device's configuration".into());
        }
        let Some((deadline, closing)) = control else {
            return files.mutation.lock().map_err(|_| {
                "File mutation state is uncertain after an interrupted operation".into()
            });
        };
        loop {
            if closing.load(std::sync::atomic::Ordering::Acquire) {
                return Err(MUTATION_SHUTTING_DOWN.into());
            }
            if Instant::now() >= deadline {
                return Err(MUTATION_DEADLINE_EXPIRED.into());
            }
            match files.mutation.try_lock() {
                Ok(guard) => {
                    if closing.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(MUTATION_SHUTTING_DOWN.into());
                    }
                    if Instant::now() >= deadline {
                        return Err(MUTATION_DEADLINE_EXPIRED.into());
                    }
                    return Ok(guard);
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    return Err(
                        "File mutation state is uncertain after an interrupted operation".into(),
                    );
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(
                        Duration::from_millis(5)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
            }
        }
    }
    fn written(path: &str, bytes: &[u8]) -> Value {
        json!({"path":path,"size_bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))})
    }
    fn write(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
        if !files.allow_write {
            return Err("File writes are disabled in this device's configuration".into());
        }
        let content = p
            .get("content")
            .and_then(Value::as_str)
            .ok_or("content must be a string")?;
        let expected = match p.get("expected_sha256") {
            None => "",
            Some(value) => value.as_str().ok_or("expected_sha256 must be a string")?,
        };
        if content.len() > MAX_FILE {
            return Err("Content exceeds 1 MiB".into());
        }
        let root = root(files, p)?;
        let path = file_path(p)?;
        normalize(path)?;
        let _lock = lock_mutation(files, control)?;
        mutation_result((|| {
            let (parent, name) = Parent::file(root, path, expected.is_empty())?;
            let previous = read_existing(&parent, &name)?;
            match previous.as_ref() {
                None if !expected.is_empty() => return Err("File no longer exists; read it again before updating".into()),
                Some(previous) if format!("{:x}",Sha256::digest(&previous.bytes)) != expected =>
                    return Err("Content conflict: supply the SHA-256 from read_file; empty hash means create only".into()),
                _ => {},
            }
            write_atomic(&parent, &name, content.as_bytes(), previous.as_ref())?;
            Ok(written(path, content.as_bytes()))
        })())
    }
    fn edit(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
        let old_value = p.get("old_text");
        if old_value.is_none_or(Value::is_null)
            || old_value == Some(&Value::Bool(false))
            || old_value.and_then(Value::as_f64) == Some(0.0)
            || old_value.and_then(Value::as_str) == Some("")
        {
            return Err("old_text cannot be empty".into());
        }
        let old = old_value
            .and_then(Value::as_str)
            .ok_or("old_text and new_text must be strings")?;
        let new = p
            .get("new_text")
            .and_then(Value::as_str)
            .ok_or("old_text and new_text must be strings")?;
        let expected = p.get("expected_sha256").and_then(Value::as_str);
        let root = root(files, p)?;
        let path = file_path(p)?;
        normalize(path)?;
        let _lock = lock_mutation(files, control)?;
        mutation_result((|| {
            let (parent, name) = Parent::file(root, path, false)?;
            let previous =
                read_existing(&parent, &name)?.ok_or("File no longer exists; read_file again")?;
            let actual = format!("{:x}", Sha256::digest(&previous.bytes));
            if expected != Some(actual.as_str()) {
                return Err("Content conflict: read_file again".into());
            }
            // from_utf8 deliberately retains the BOM, matching ignoreBOM:true.
            let text =
                std::str::from_utf8(&previous.bytes).map_err(|_| "File is not valid UTF-8")?;
            let first = text.find(old).ok_or("old_text must match exactly once")?;
            let end = first + old.len();
            if text[end..].contains(old) {
                return Err("old_text must match exactly once".into());
            }
            let size = text
                .len()
                .checked_sub(old.len())
                .and_then(|n| n.checked_add(new.len()))
                .ok_or("Content exceeds 1 MiB")?;
            if size > MAX_FILE {
                return Err("Content exceeds 1 MiB".into());
            }
            let mut edited = String::with_capacity(size);
            edited.push_str(&text[..first]);
            edited.push_str(new);
            edited.push_str(&text[end..]);
            write_atomic(&parent, &name, edited.as_bytes(), Some(&previous))?;
            Ok(written(path, edited.as_bytes()))
        })())
    }
    fn mkdir(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
        if !files.allow_write {
            return Err("File writes are disabled".into());
        }
        let root = root(files, p)?;
        let path = file_path(p)?;
        normalize(path)?;
        let _lock = lock_mutation(files, control)?;
        mutation_result(make_directories(root, path).map(|()| json!({"created":path})))
    }

    fn move_file(files: &Files, p: &Value, control: Control<'_>) -> Result<Value, String> {
        let modern = p.get("source").is_some() || p.get("destination").is_some();
        if modern
            && ["path", "destination_root", "destination_path"]
                .iter()
                .any(|key| p.get(key).is_some())
        {
            return Err("Mixed move parameter formats are not allowed".into());
        }
        let move_parameter = |name| {
            p.get(name)
                .and_then(Value::as_str)
                .ok_or("Invalid move parameters")
        };
        let root_name = move_parameter("root")?;
        let (source, destination_root, destination) = if modern {
            (
                move_parameter("source")?,
                root_name,
                move_parameter("destination")?,
            )
        } else {
            (
                move_parameter("path")?,
                move_parameter("destination_root")?,
                move_parameter("destination_path")?,
            )
        };
        if !files.allow_write {
            return Err("File writes are disabled in this device's configuration".into());
        }
        if root_name != destination_root {
            return Err("Moving files across roots is disabled".into());
        }
        let root = root(files, p)?;
        normalize(source)?;
        normalize(destination)?;
        let _lock = lock_mutation(files, control)?;
        mutation_result((|| {
            let (source_parent, source_name) = Parent::file(root, source, false)?;
            let (destination_parent, destination_name) = Parent::file(root, destination, false)?;
            let source_file = open_beneath(
                &source_parent.directory,
                &source_name,
                libc::O_RDONLY | libc::O_NONBLOCK,
            )?;
            let source_metadata = source_file.metadata().map_err(io_error)?;
            if !source_metadata.is_file() {
                return Err("Only regular files can be moved".into());
            }
            verify_file(&source_parent, &source_name, &source_metadata)?;
            destination_parent.verify()?;
            if stat_at(&destination_parent.directory, &destination_name)?.is_some() {
                return Err("Destination already exists".into());
            }
            if source_metadata.dev()
                != destination_parent
                    .directory
                    .metadata()
                    .map_err(io_error)?
                    .dev()
            {
                return Err("Moving files across filesystems is disabled".into());
            }
            let source_c = cstring(&source_name)?;
            let destination_c = cstring(&destination_name)?;
            // SAFETY: both directory descriptors are live and each string is a
            // single component. NOREPLACE atomically rejects any destination;
            // rename does not follow either leaf. There is no weaker fallback.
            #[cfg(target_os = "linux")]
            let result = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    source_parent.directory.as_raw_fd(),
                    source_c.as_ptr(),
                    destination_parent.directory.as_raw_fd(),
                    destination_c.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let result = unsafe {
                libc::renameatx_np(
                    source_parent.directory.as_raw_fd(),
                    source_c.as_ptr(),
                    destination_parent.directory.as_raw_fd(),
                    destination_c.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EXDEV) {
                    return Err("Moving files across filesystems is disabled".into());
                }
                if error.raw_os_error() == Some(libc::EEXIST) {
                    return Err("Destination already exists".into());
                }
                return Err(io_error(error));
            }
            crate::storage::sync_directory(&source_parent.directory).map_err(io_error)?;
            crate::storage::sync_directory(&destination_parent.directory).map_err(io_error)?;
            source_parent.verify()?;
            destination_parent.verify()?;
            let moved = stat_at(&destination_parent.directory, &destination_name)?
                .ok_or("Moved file disappeared")?;
            if moved.st_mode & libc::S_IFMT != libc::S_IFREG
                || !same_inode(&moved, &source_metadata)
            {
                return Err("Moved file changed".into());
            }
            Ok(json!({"path":source,"destination_path":destination,"moved":true}))
        })())
    }

    pub(super) fn dispatch(
        files: &Files,
        operation: &str,
        p: &Value,
        control: Control<'_>,
    ) -> Result<Value, String> {
        if !p.is_object() {
            return Err("Parameters must be an object".into());
        }
        match operation {
            "read_file" => read(files, p),
            "file_info" => info(files, p),
            "read_file_chunk" => chunk(files, p),
            "list_directory" => list(files, p),
            "read_multiple_files" => multiple(files, p),
            "search_files" => search(files, p),
            "write_file" => write(files, p, control),
            "edit_file" => edit(files, p, control),
            "make_directory" => mkdir(files, p, control),
            "move_file" => move_file(files, p, control),
            _ => Err("Unsupported file operation".into()),
        }
    }

    // Exact exception mappings from src/casefold.ts (Unicode 13).
    pub(super) fn casefold(value: &str) -> String {
        let mut result = String::new();
        for c in value.chars() {
            match c {
                '\u{b5}' => result.push('\u{3bc}'),
                '\u{df}' => result.push_str("\u{73}\u{73}"),
                '\u{149}' => result.push_str("\u{2bc}\u{6e}"),
                '\u{17f}' => result.push('\u{73}'),
                '\u{1f0}' => result.push_str("\u{6a}\u{30c}"),
                '\u{345}' => result.push('\u{3b9}'),
                '\u{390}' => result.push_str("\u{3b9}\u{308}\u{301}"),
                '\u{3b0}' => result.push_str("\u{3c5}\u{308}\u{301}"),
                '\u{3c2}' => result.push('\u{3c3}'),
                '\u{3d0}' => result.push('\u{3b2}'),
                '\u{3d1}' => result.push('\u{3b8}'),
                '\u{3d5}' => result.push('\u{3c6}'),
                '\u{3d6}' => result.push('\u{3c0}'),
                '\u{3f0}' => result.push('\u{3ba}'),
                '\u{3f1}' => result.push('\u{3c1}'),
                '\u{3f5}' => result.push('\u{3b5}'),
                '\u{587}' => result.push_str("\u{565}\u{582}"),
                '\u{13a0}' => result.push('\u{13a0}'),
                '\u{13a1}' => result.push('\u{13a1}'),
                '\u{13a2}' => result.push('\u{13a2}'),
                '\u{13a3}' => result.push('\u{13a3}'),
                '\u{13a4}' => result.push('\u{13a4}'),
                '\u{13a5}' => result.push('\u{13a5}'),
                '\u{13a6}' => result.push('\u{13a6}'),
                '\u{13a7}' => result.push('\u{13a7}'),
                '\u{13a8}' => result.push('\u{13a8}'),
                '\u{13a9}' => result.push('\u{13a9}'),
                '\u{13aa}' => result.push('\u{13aa}'),
                '\u{13ab}' => result.push('\u{13ab}'),
                '\u{13ac}' => result.push('\u{13ac}'),
                '\u{13ad}' => result.push('\u{13ad}'),
                '\u{13ae}' => result.push('\u{13ae}'),
                '\u{13af}' => result.push('\u{13af}'),
                '\u{13b0}' => result.push('\u{13b0}'),
                '\u{13b1}' => result.push('\u{13b1}'),
                '\u{13b2}' => result.push('\u{13b2}'),
                '\u{13b3}' => result.push('\u{13b3}'),
                '\u{13b4}' => result.push('\u{13b4}'),
                '\u{13b5}' => result.push('\u{13b5}'),
                '\u{13b6}' => result.push('\u{13b6}'),
                '\u{13b7}' => result.push('\u{13b7}'),
                '\u{13b8}' => result.push('\u{13b8}'),
                '\u{13b9}' => result.push('\u{13b9}'),
                '\u{13ba}' => result.push('\u{13ba}'),
                '\u{13bb}' => result.push('\u{13bb}'),
                '\u{13bc}' => result.push('\u{13bc}'),
                '\u{13bd}' => result.push('\u{13bd}'),
                '\u{13be}' => result.push('\u{13be}'),
                '\u{13bf}' => result.push('\u{13bf}'),
                '\u{13c0}' => result.push('\u{13c0}'),
                '\u{13c1}' => result.push('\u{13c1}'),
                '\u{13c2}' => result.push('\u{13c2}'),
                '\u{13c3}' => result.push('\u{13c3}'),
                '\u{13c4}' => result.push('\u{13c4}'),
                '\u{13c5}' => result.push('\u{13c5}'),
                '\u{13c6}' => result.push('\u{13c6}'),
                '\u{13c7}' => result.push('\u{13c7}'),
                '\u{13c8}' => result.push('\u{13c8}'),
                '\u{13c9}' => result.push('\u{13c9}'),
                '\u{13ca}' => result.push('\u{13ca}'),
                '\u{13cb}' => result.push('\u{13cb}'),
                '\u{13cc}' => result.push('\u{13cc}'),
                '\u{13cd}' => result.push('\u{13cd}'),
                '\u{13ce}' => result.push('\u{13ce}'),
                '\u{13cf}' => result.push('\u{13cf}'),
                '\u{13d0}' => result.push('\u{13d0}'),
                '\u{13d1}' => result.push('\u{13d1}'),
                '\u{13d2}' => result.push('\u{13d2}'),
                '\u{13d3}' => result.push('\u{13d3}'),
                '\u{13d4}' => result.push('\u{13d4}'),
                '\u{13d5}' => result.push('\u{13d5}'),
                '\u{13d6}' => result.push('\u{13d6}'),
                '\u{13d7}' => result.push('\u{13d7}'),
                '\u{13d8}' => result.push('\u{13d8}'),
                '\u{13d9}' => result.push('\u{13d9}'),
                '\u{13da}' => result.push('\u{13da}'),
                '\u{13db}' => result.push('\u{13db}'),
                '\u{13dc}' => result.push('\u{13dc}'),
                '\u{13dd}' => result.push('\u{13dd}'),
                '\u{13de}' => result.push('\u{13de}'),
                '\u{13df}' => result.push('\u{13df}'),
                '\u{13e0}' => result.push('\u{13e0}'),
                '\u{13e1}' => result.push('\u{13e1}'),
                '\u{13e2}' => result.push('\u{13e2}'),
                '\u{13e3}' => result.push('\u{13e3}'),
                '\u{13e4}' => result.push('\u{13e4}'),
                '\u{13e5}' => result.push('\u{13e5}'),
                '\u{13e6}' => result.push('\u{13e6}'),
                '\u{13e7}' => result.push('\u{13e7}'),
                '\u{13e8}' => result.push('\u{13e8}'),
                '\u{13e9}' => result.push('\u{13e9}'),
                '\u{13ea}' => result.push('\u{13ea}'),
                '\u{13eb}' => result.push('\u{13eb}'),
                '\u{13ec}' => result.push('\u{13ec}'),
                '\u{13ed}' => result.push('\u{13ed}'),
                '\u{13ee}' => result.push('\u{13ee}'),
                '\u{13ef}' => result.push('\u{13ef}'),
                '\u{13f0}' => result.push('\u{13f0}'),
                '\u{13f1}' => result.push('\u{13f1}'),
                '\u{13f2}' => result.push('\u{13f2}'),
                '\u{13f3}' => result.push('\u{13f3}'),
                '\u{13f4}' => result.push('\u{13f4}'),
                '\u{13f5}' => result.push('\u{13f5}'),
                '\u{13f8}' => result.push('\u{13f0}'),
                '\u{13f9}' => result.push('\u{13f1}'),
                '\u{13fa}' => result.push('\u{13f2}'),
                '\u{13fb}' => result.push('\u{13f3}'),
                '\u{13fc}' => result.push('\u{13f4}'),
                '\u{13fd}' => result.push('\u{13f5}'),
                '\u{1c80}' => result.push('\u{432}'),
                '\u{1c81}' => result.push('\u{434}'),
                '\u{1c82}' => result.push('\u{43e}'),
                '\u{1c83}' => result.push('\u{441}'),
                '\u{1c84}' => result.push('\u{442}'),
                '\u{1c85}' => result.push('\u{442}'),
                '\u{1c86}' => result.push('\u{44a}'),
                '\u{1c87}' => result.push('\u{463}'),
                '\u{1c88}' => result.push('\u{a64b}'),
                '\u{1e96}' => result.push_str("\u{68}\u{331}"),
                '\u{1e97}' => result.push_str("\u{74}\u{308}"),
                '\u{1e98}' => result.push_str("\u{77}\u{30a}"),
                '\u{1e99}' => result.push_str("\u{79}\u{30a}"),
                '\u{1e9a}' => result.push_str("\u{61}\u{2be}"),
                '\u{1e9b}' => result.push('\u{1e61}'),
                '\u{1e9e}' => result.push_str("\u{73}\u{73}"),
                '\u{1f50}' => result.push_str("\u{3c5}\u{313}"),
                '\u{1f52}' => result.push_str("\u{3c5}\u{313}\u{300}"),
                '\u{1f54}' => result.push_str("\u{3c5}\u{313}\u{301}"),
                '\u{1f56}' => result.push_str("\u{3c5}\u{313}\u{342}"),
                '\u{1f80}' => result.push_str("\u{1f00}\u{3b9}"),
                '\u{1f81}' => result.push_str("\u{1f01}\u{3b9}"),
                '\u{1f82}' => result.push_str("\u{1f02}\u{3b9}"),
                '\u{1f83}' => result.push_str("\u{1f03}\u{3b9}"),
                '\u{1f84}' => result.push_str("\u{1f04}\u{3b9}"),
                '\u{1f85}' => result.push_str("\u{1f05}\u{3b9}"),
                '\u{1f86}' => result.push_str("\u{1f06}\u{3b9}"),
                '\u{1f87}' => result.push_str("\u{1f07}\u{3b9}"),
                '\u{1f88}' => result.push_str("\u{1f00}\u{3b9}"),
                '\u{1f89}' => result.push_str("\u{1f01}\u{3b9}"),
                '\u{1f8a}' => result.push_str("\u{1f02}\u{3b9}"),
                '\u{1f8b}' => result.push_str("\u{1f03}\u{3b9}"),
                '\u{1f8c}' => result.push_str("\u{1f04}\u{3b9}"),
                '\u{1f8d}' => result.push_str("\u{1f05}\u{3b9}"),
                '\u{1f8e}' => result.push_str("\u{1f06}\u{3b9}"),
                '\u{1f8f}' => result.push_str("\u{1f07}\u{3b9}"),
                '\u{1f90}' => result.push_str("\u{1f20}\u{3b9}"),
                '\u{1f91}' => result.push_str("\u{1f21}\u{3b9}"),
                '\u{1f92}' => result.push_str("\u{1f22}\u{3b9}"),
                '\u{1f93}' => result.push_str("\u{1f23}\u{3b9}"),
                '\u{1f94}' => result.push_str("\u{1f24}\u{3b9}"),
                '\u{1f95}' => result.push_str("\u{1f25}\u{3b9}"),
                '\u{1f96}' => result.push_str("\u{1f26}\u{3b9}"),
                '\u{1f97}' => result.push_str("\u{1f27}\u{3b9}"),
                '\u{1f98}' => result.push_str("\u{1f20}\u{3b9}"),
                '\u{1f99}' => result.push_str("\u{1f21}\u{3b9}"),
                '\u{1f9a}' => result.push_str("\u{1f22}\u{3b9}"),
                '\u{1f9b}' => result.push_str("\u{1f23}\u{3b9}"),
                '\u{1f9c}' => result.push_str("\u{1f24}\u{3b9}"),
                '\u{1f9d}' => result.push_str("\u{1f25}\u{3b9}"),
                '\u{1f9e}' => result.push_str("\u{1f26}\u{3b9}"),
                '\u{1f9f}' => result.push_str("\u{1f27}\u{3b9}"),
                '\u{1fa0}' => result.push_str("\u{1f60}\u{3b9}"),
                '\u{1fa1}' => result.push_str("\u{1f61}\u{3b9}"),
                '\u{1fa2}' => result.push_str("\u{1f62}\u{3b9}"),
                '\u{1fa3}' => result.push_str("\u{1f63}\u{3b9}"),
                '\u{1fa4}' => result.push_str("\u{1f64}\u{3b9}"),
                '\u{1fa5}' => result.push_str("\u{1f65}\u{3b9}"),
                '\u{1fa6}' => result.push_str("\u{1f66}\u{3b9}"),
                '\u{1fa7}' => result.push_str("\u{1f67}\u{3b9}"),
                '\u{1fa8}' => result.push_str("\u{1f60}\u{3b9}"),
                '\u{1fa9}' => result.push_str("\u{1f61}\u{3b9}"),
                '\u{1faa}' => result.push_str("\u{1f62}\u{3b9}"),
                '\u{1fab}' => result.push_str("\u{1f63}\u{3b9}"),
                '\u{1fac}' => result.push_str("\u{1f64}\u{3b9}"),
                '\u{1fad}' => result.push_str("\u{1f65}\u{3b9}"),
                '\u{1fae}' => result.push_str("\u{1f66}\u{3b9}"),
                '\u{1faf}' => result.push_str("\u{1f67}\u{3b9}"),
                '\u{1fb2}' => result.push_str("\u{1f70}\u{3b9}"),
                '\u{1fb3}' => result.push_str("\u{3b1}\u{3b9}"),
                '\u{1fb4}' => result.push_str("\u{3ac}\u{3b9}"),
                '\u{1fb6}' => result.push_str("\u{3b1}\u{342}"),
                '\u{1fb7}' => result.push_str("\u{3b1}\u{342}\u{3b9}"),
                '\u{1fbc}' => result.push_str("\u{3b1}\u{3b9}"),
                '\u{1fbe}' => result.push('\u{3b9}'),
                '\u{1fc2}' => result.push_str("\u{1f74}\u{3b9}"),
                '\u{1fc3}' => result.push_str("\u{3b7}\u{3b9}"),
                '\u{1fc4}' => result.push_str("\u{3ae}\u{3b9}"),
                '\u{1fc6}' => result.push_str("\u{3b7}\u{342}"),
                '\u{1fc7}' => result.push_str("\u{3b7}\u{342}\u{3b9}"),
                '\u{1fcc}' => result.push_str("\u{3b7}\u{3b9}"),
                '\u{1fd2}' => result.push_str("\u{3b9}\u{308}\u{300}"),
                '\u{1fd3}' => result.push_str("\u{3b9}\u{308}\u{301}"),
                '\u{1fd6}' => result.push_str("\u{3b9}\u{342}"),
                '\u{1fd7}' => result.push_str("\u{3b9}\u{308}\u{342}"),
                '\u{1fe2}' => result.push_str("\u{3c5}\u{308}\u{300}"),
                '\u{1fe3}' => result.push_str("\u{3c5}\u{308}\u{301}"),
                '\u{1fe4}' => result.push_str("\u{3c1}\u{313}"),
                '\u{1fe6}' => result.push_str("\u{3c5}\u{342}"),
                '\u{1fe7}' => result.push_str("\u{3c5}\u{308}\u{342}"),
                '\u{1ff2}' => result.push_str("\u{1f7c}\u{3b9}"),
                '\u{1ff3}' => result.push_str("\u{3c9}\u{3b9}"),
                '\u{1ff4}' => result.push_str("\u{3ce}\u{3b9}"),
                '\u{1ff6}' => result.push_str("\u{3c9}\u{342}"),
                '\u{1ff7}' => result.push_str("\u{3c9}\u{342}\u{3b9}"),
                '\u{1ffc}' => result.push_str("\u{3c9}\u{3b9}"),
                '\u{ab70}' => result.push('\u{13a0}'),
                '\u{ab71}' => result.push('\u{13a1}'),
                '\u{ab72}' => result.push('\u{13a2}'),
                '\u{ab73}' => result.push('\u{13a3}'),
                '\u{ab74}' => result.push('\u{13a4}'),
                '\u{ab75}' => result.push('\u{13a5}'),
                '\u{ab76}' => result.push('\u{13a6}'),
                '\u{ab77}' => result.push('\u{13a7}'),
                '\u{ab78}' => result.push('\u{13a8}'),
                '\u{ab79}' => result.push('\u{13a9}'),
                '\u{ab7a}' => result.push('\u{13aa}'),
                '\u{ab7b}' => result.push('\u{13ab}'),
                '\u{ab7c}' => result.push('\u{13ac}'),
                '\u{ab7d}' => result.push('\u{13ad}'),
                '\u{ab7e}' => result.push('\u{13ae}'),
                '\u{ab7f}' => result.push('\u{13af}'),
                '\u{ab80}' => result.push('\u{13b0}'),
                '\u{ab81}' => result.push('\u{13b1}'),
                '\u{ab82}' => result.push('\u{13b2}'),
                '\u{ab83}' => result.push('\u{13b3}'),
                '\u{ab84}' => result.push('\u{13b4}'),
                '\u{ab85}' => result.push('\u{13b5}'),
                '\u{ab86}' => result.push('\u{13b6}'),
                '\u{ab87}' => result.push('\u{13b7}'),
                '\u{ab88}' => result.push('\u{13b8}'),
                '\u{ab89}' => result.push('\u{13b9}'),
                '\u{ab8a}' => result.push('\u{13ba}'),
                '\u{ab8b}' => result.push('\u{13bb}'),
                '\u{ab8c}' => result.push('\u{13bc}'),
                '\u{ab8d}' => result.push('\u{13bd}'),
                '\u{ab8e}' => result.push('\u{13be}'),
                '\u{ab8f}' => result.push('\u{13bf}'),
                '\u{ab90}' => result.push('\u{13c0}'),
                '\u{ab91}' => result.push('\u{13c1}'),
                '\u{ab92}' => result.push('\u{13c2}'),
                '\u{ab93}' => result.push('\u{13c3}'),
                '\u{ab94}' => result.push('\u{13c4}'),
                '\u{ab95}' => result.push('\u{13c5}'),
                '\u{ab96}' => result.push('\u{13c6}'),
                '\u{ab97}' => result.push('\u{13c7}'),
                '\u{ab98}' => result.push('\u{13c8}'),
                '\u{ab99}' => result.push('\u{13c9}'),
                '\u{ab9a}' => result.push('\u{13ca}'),
                '\u{ab9b}' => result.push('\u{13cb}'),
                '\u{ab9c}' => result.push('\u{13cc}'),
                '\u{ab9d}' => result.push('\u{13cd}'),
                '\u{ab9e}' => result.push('\u{13ce}'),
                '\u{ab9f}' => result.push('\u{13cf}'),
                '\u{aba0}' => result.push('\u{13d0}'),
                '\u{aba1}' => result.push('\u{13d1}'),
                '\u{aba2}' => result.push('\u{13d2}'),
                '\u{aba3}' => result.push('\u{13d3}'),
                '\u{aba4}' => result.push('\u{13d4}'),
                '\u{aba5}' => result.push('\u{13d5}'),
                '\u{aba6}' => result.push('\u{13d6}'),
                '\u{aba7}' => result.push('\u{13d7}'),
                '\u{aba8}' => result.push('\u{13d8}'),
                '\u{aba9}' => result.push('\u{13d9}'),
                '\u{abaa}' => result.push('\u{13da}'),
                '\u{abab}' => result.push('\u{13db}'),
                '\u{abac}' => result.push('\u{13dc}'),
                '\u{abad}' => result.push('\u{13dd}'),
                '\u{abae}' => result.push('\u{13de}'),
                '\u{abaf}' => result.push('\u{13df}'),
                '\u{abb0}' => result.push('\u{13e0}'),
                '\u{abb1}' => result.push('\u{13e1}'),
                '\u{abb2}' => result.push('\u{13e2}'),
                '\u{abb3}' => result.push('\u{13e3}'),
                '\u{abb4}' => result.push('\u{13e4}'),
                '\u{abb5}' => result.push('\u{13e5}'),
                '\u{abb6}' => result.push('\u{13e6}'),
                '\u{abb7}' => result.push('\u{13e7}'),
                '\u{abb8}' => result.push('\u{13e8}'),
                '\u{abb9}' => result.push('\u{13e9}'),
                '\u{abba}' => result.push('\u{13ea}'),
                '\u{abbb}' => result.push('\u{13eb}'),
                '\u{abbc}' => result.push('\u{13ec}'),
                '\u{abbd}' => result.push('\u{13ed}'),
                '\u{abbe}' => result.push('\u{13ee}'),
                '\u{abbf}' => result.push('\u{13ef}'),
                '\u{fb00}' => result.push_str("\u{66}\u{66}"),
                '\u{fb01}' => result.push_str("\u{66}\u{69}"),
                '\u{fb02}' => result.push_str("\u{66}\u{6c}"),
                '\u{fb03}' => result.push_str("\u{66}\u{66}\u{69}"),
                '\u{fb04}' => result.push_str("\u{66}\u{66}\u{6c}"),
                '\u{fb05}' => result.push_str("\u{73}\u{74}"),
                '\u{fb06}' => result.push_str("\u{73}\u{74}"),
                '\u{fb13}' => result.push_str("\u{574}\u{576}"),
                '\u{fb14}' => result.push_str("\u{574}\u{565}"),
                '\u{fb15}' => result.push_str("\u{574}\u{56b}"),
                '\u{fb16}' => result.push_str("\u{57e}\u{576}"),
                '\u{fb17}' => result.push_str("\u{574}\u{56d}"),
                _ => result.extend(c.to_lowercase()),
            }
        }
        result
    }
}
