//! Bounded Protocol v1 file operations.
//!
//! Linux requires openat2 (Linux 5.6+). Experimental macOS uses directory-relative
//! opens with kernel NOFOLLOW_ANY plus descriptor path checks. Both retain
//! configured root identities and reject symbolic links in request paths.
//! Experimental Windows retains ancestor handles and uses handle-relative NT
//! filesystem operations. Both experimental backends remain disabled by default.
use serde_json::Value;
use std::collections::BTreeMap;

#[cfg(all(target_os = "macos", feature = "experimental-macos-files"))]
mod macos;

pub const MUTATION_DEADLINE_EXPIRED: &str = "File mutation deadline expired before execution";
pub const MUTATION_SHUTTING_DOWN: &str = "Agent is shutting down; file mutation was not started";

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files"),
    all(windows, feature = "experimental-windows-files")
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
    all(target_os = "macos", feature = "experimental-macos-files"),
    all(windows, feature = "experimental-windows-files")
)))]
pub const FILE_OPERATIONS: &[&str] = &[];

pub struct Files {
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files"),
        all(windows, feature = "experimental-windows-files")
    ))]
    roots: BTreeMap<String, native::Root>,
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files"),
        all(windows, feature = "experimental-windows-files")
    ))]
    allow_write: bool,
    // The RPC admission limit bounds callers waiting on this single lock.
    // Ownership stays with the actual blocking operation through commit/cleanup.
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "experimental-macos-files"),
        all(windows, feature = "experimental-windows-files")
    ))]
    mutation: std::sync::Mutex<()>,
}

// Keep the configured root handles as well as the selected directory alive
// until Windows has created and resumed the child in that directory.
#[cfg(all(windows, feature = "experimental-windows-processes"))]
pub(crate) struct WindowsWorkingDirectory {
    _roots: std::sync::Arc<Files>,
    _opened: native::WindowsDirectory,
}

pub(crate) struct SearchEntry {
    pub name: String,
    pub kind: &'static str,
}

impl Files {
    pub fn new(roots: &BTreeMap<String, String>, allow_write: bool) -> Result<Self, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            let mut handles = BTreeMap::new();
            for (name, path) in roots {
                handles.insert(name.clone(), native::Root::new(path)?);
            }
            Ok(Self {
                roots: handles,
                allow_write,
                mutation: std::sync::Mutex::new(()),
            })
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        )))]
        {
            let _ = (roots, allow_write);
            Err("Secure native file operations are unsupported on this platform".into())
        }
    }

    pub fn dispatch(&self, operation: &str, params: &Value) -> Result<Value, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::dispatch(self, operation, params, None)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
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
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::dispatch(self, operation, params, Some((deadline, closing)))
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
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
            native::working_directory(self, root, path)
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

    #[cfg(all(windows, feature = "experimental-windows-processes"))]
    pub(crate) fn windows_working_directory(
        self: &std::sync::Arc<Self>,
        root: &str,
        path: &str,
    ) -> Result<WindowsWorkingDirectory, String> {
        let opened = native::windows_working_directory(self, root, path)?;
        Ok(WindowsWorkingDirectory {
            _roots: std::sync::Arc::clone(self),
            _opened: opened,
        })
    }

    // Background searches validate the request before allocating a task, but
    // report failures opening an otherwise valid directory on the task itself.
    pub(crate) fn validate_search_path(&self, root: &str, path: &str) -> Result<(), String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::validate_search_path(self, root, path)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        )))]
        {
            let _ = (root, path);
            Err("Secure native search is unsupported on this platform".into())
        }
    }

    pub(crate) fn check_search_directory(&self, root: &str, path: &str) -> Result<(), String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::check_search_directory(self, root, path)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
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
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::search_entries(self, root, path, remaining, cancelled, deadline)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        )))]
        {
            let _ = (root, path, remaining, cancelled, deadline);
            Err("Secure native search is unsupported on this platform".into())
        }
    }

    pub(crate) fn search_read(&self, root: &str, path: &str) -> Result<Vec<u8>, String> {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::search_read(self, root, path)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        )))]
        {
            let _ = (root, path);
            Err("Secure native search is unsupported on this platform".into())
        }
    }

    pub(crate) fn search_fold(value: &str) -> String {
        #[cfg(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        ))]
        {
            native::casefold(value)
        }
        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "macos", feature = "experimental-macos-files"),
            all(windows, feature = "experimental-windows-files")
        )))]
        {
            value.to_lowercase()
        }
    }

    #[cfg(all(
        test,
        any(
            target_os = "linux",
            all(
                target_os = "macos",
                feature = "experimental-macos-files",
                feature = "experimental-macos-processes",
                feature = "experimental-macos-audit"
            )
        )
    ))]
    pub(crate) fn hold_mutation_for_test(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutation.lock().unwrap()
    }
}

#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "experimental-macos-files"),
    all(windows, feature = "experimental-windows-files")
))]
mod native;

#[cfg(all(test, windows, not(feature = "experimental-windows-files")))]
mod windows_default_tests {
    use super::*;
    #[test]
    fn files_and_search_remain_unavailable_without_the_feature() {
        let path = std::env::temp_dir().join(format!(
            "rdh-default-files-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        assert!(
            Files::new(
                &BTreeMap::from([("work".into(), path.to_str().unwrap().into())]),
                true
            )
            .is_err()
        );
        assert!(FILE_OPERATIONS.is_empty());
        assert!(crate::searches::SEARCH_OPERATIONS.is_empty());
        assert!(!path.exists());
    }
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
        all(target_os = "macos", feature = "experimental-macos-files"),
        all(windows, feature = "experimental-windows-files")
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
                &crate::config::local_roots(&[format!("work={}", fixture.path.display())]).unwrap(),
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
            self.files.roots.clear();
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
    #[cfg(unix)]
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
            assert_eq!(native::js_number(value), expected);
        }
        assert_eq!(
            native::js_unsigned(9_007_199_254_740_993),
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
        all(target_os = "macos", feature = "experimental-macos-files"),
        all(windows, feature = "experimental-windows-files")
    )
))]
mod mutation_tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};

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
                &crate::config::local_roots(&[format!("work={}", fixture.path.display())]).unwrap(),
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
            self.files.roots.clear();
            let _ = fs::remove_dir_all(&self.path);
        }
    }
    fn digest(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
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
    #[cfg(unix)]
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
