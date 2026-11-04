use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub fn config(base: &Path) -> PathBuf {
    rdh_agent::config::ProfilePaths::resolve(
        base,
        rdh_agent::config::DEFAULT_SERVER,
        "default",
        None,
    )
    .unwrap()
    .destination
}

pub fn seed(base: &Path, server: &str, roots: Value, token: &str, response: Value) -> PathBuf {
    let config = config(base);
    rdh_agent::storage::ensure_parent(&config).unwrap();
    rdh_agent::storage::write_private_replace(
        &base.join("remote-device-hub/settings.json"),
        &serde_json::to_vec(&json!({"roots":roots})).unwrap(),
    )
    .unwrap();
    rdh_agent::storage::write_private_replace(
        &PathBuf::from(format!("{}.pairing.json", config.display())),
        &serde_json::to_vec(
            &json!({"server":server,"token":token,"roots":roots,"response":response}),
        )
        .unwrap(),
    )
    .unwrap();
    config
}
