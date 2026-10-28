use std::cmp::Ordering;
use std::path::PathBuf;

use rdh_agent::{
    config::sha256_hex,
    update::{MAX_BINARY_BYTES, compare_versions},
};
use serde_json::Value;

/// The fixture must be independently compiled. Replacing version bytes in an
/// ELF file leaves optimized immediates and comparisons at the original value.
pub fn load() -> (String, Vec<u8>) {
    let directory = PathBuf::from(std::env::var_os("RDH_NATIVE_TEST_CANDIDATE_DIR").expect(
        "Build a candidate with `cargo run --locked -p xtask -- native-test-candidate --target TARGET --out ABSOLUTE_NEW_DIRECTORY`, then set RDH_NATIVE_TEST_CANDIDATE_DIR to that directory",
    ));
    assert!(
        directory.is_absolute(),
        "Native candidate directory must be absolute"
    );
    let manifest = directory.join("candidate.json");
    assert!(std::fs::metadata(&manifest).unwrap().len() <= 32 * 1024);
    let manifest: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    assert_eq!(manifest["schema"], 1);
    assert_eq!(manifest["kind"], "native-test-candidate");
    assert_eq!(manifest["source_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["release_ready"], false);
    let target = format!(
        "{}-unknown-linux-{}",
        std::env::consts::ARCH,
        if cfg!(target_env = "musl") {
            "musl"
        } else {
            "gnu"
        }
    );
    assert_eq!(manifest["target"], target);
    let version = manifest["version"].as_str().unwrap().to_owned();
    assert_eq!(
        compare_versions(&version, env!("CARGO_PKG_VERSION")).unwrap(),
        Ordering::Greater
    );
    let binary = directory.join("rdh");
    let metadata = std::fs::metadata(&binary).unwrap();
    assert!(metadata.is_file() && metadata.len() > 0 && metadata.len() <= MAX_BINARY_BYTES);
    let bytes = std::fs::read(binary).unwrap();
    assert_eq!(manifest["binary"]["bytes"], bytes.len());
    assert_eq!(manifest["binary"]["sha256"], sha256_hex(&bytes));
    (version, bytes)
}
