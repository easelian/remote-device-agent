//! Use the extracted development artifact for installation acceptance in CI.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn path() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let Some(directory) = std::env::var_os("RDH_NATIVE_ARTIFACT_DIR") else {
            return PathBuf::from(env!("CARGO_BIN_EXE_rdh"));
        };
        let directory = PathBuf::from(directory);
        assert!(
            directory.is_absolute(),
            "Artifact directory must be absolute"
        );
        let manifest = directory.join("manifest.json");
        assert!(std::fs::metadata(&manifest).unwrap().len() <= 1024 * 1024);
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        assert_eq!(manifest["kind"], "native-agent-development");
        assert_eq!(manifest["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["features"], "all");
        assert_eq!(manifest["runtime_self_check"], true);
        let filename = if cfg!(windows) { "rdh.exe" } else { "rdh" };
        assert_eq!(manifest["binary"]["path"], filename);
        let binary = directory.join(filename);
        let metadata = std::fs::symlink_metadata(&binary).unwrap();
        assert!(metadata.file_type().is_file());
        assert!(metadata.len() <= rdh_agent::update::MAX_BINARY_BYTES);
        assert_eq!(manifest["binary"]["bytes"], metadata.len());
        assert_eq!(
            manifest["binary"]["sha256"],
            rdh_agent::config::sha256_hex(&std::fs::read(&binary).unwrap())
        );
        eprintln!("Using verified extracted artifact: {}", binary.display());
        binary
    })
}
