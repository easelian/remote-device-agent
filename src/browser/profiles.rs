//! Named browser data is private to the authenticated AI connection and device binding.
use super::{Failure, Principal, Result, io_error};
use crate::storage;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const MAX_PROFILES: usize = 16;
#[cfg_attr(
    not(all(
        target_os = "linux",
        target_arch = "x86_64",
        feature = "experimental-browser-bidi"
    )),
    allow(dead_code)
)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    V1,
    CdpV2,
    BidiV2,
}
impl Scope {
    fn namespace(self) -> Option<&'static str> {
        match self {
            Self::V1 => None,
            Self::CdpV2 => Some("rdh-browser-profile-v2-chromium"),
            Self::BidiV2 => Some("rdh-browser-profile-v2-firefox"),
        }
    }
    pub fn id(self, principal: &Principal, name: &str) -> String {
        crate::config::sha256_hex(
            &serde_json::to_vec(&json!([
                self.namespace().unwrap_or("rdh-browser-profile-v1"),
                principal.owner,
                name
            ]))
            .unwrap(),
        )
    }
}

#[derive(Default)]
pub struct Profiles {
    catalog: Mutex<()>,
    leased: Arc<Mutex<HashSet<String>>>,
}

struct Lease {
    id: String,
    leased: Arc<Mutex<HashSet<String>>>,
    release: bool,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.release {
            self.leased.lock().unwrap().remove(&self.id);
        }
    }
}

pub struct NamedProfile {
    pub data: PathBuf,
    lease: Lease,
}
impl NamedProfile {
    pub fn retain(&mut self) {
        self.lease.release = false;
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    namespace: Option<String>,
    id: String,
    owner: String,
    user: String,
    name: String,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= 64
        && !name.chars().any(char::is_control)
}
pub fn id(principal: &Principal, name: &str) -> String {
    crate::config::sha256_hex(
        &serde_json::to_vec(&json!(["rdh-browser-profile-v1", principal.owner, name])).unwrap(),
    )
}
fn private_dir(path: &Path) -> Result<()> {
    match storage::create_private_dir(path) {
        Ok(_) => Ok(()),
        Err(crate::error::Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            storage::private_metadata(path, true)
                .map(|_| ())
                .map_err(io_error)
        }
        Err(error) => Err(io_error(error)),
    }
}
fn read(path: &Path, scope: Scope) -> Result<Metadata> {
    storage::private_metadata(path, true).map_err(io_error)?;
    let bytes = storage::read_private(&path.join("metadata.json")).map_err(io_error)?;
    if bytes.len() > 2048 {
        return Err(Failure::new("browser_profile_unavailable"));
    }
    let meta: Metadata = serde_json::from_slice(&bytes).map_err(io_error)?;
    let hex = |s: &str| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if meta.namespace.as_deref() != scope.namespace()
        || meta.version != 1
        || !hex(&meta.id)
        || !hex(&meta.owner)
        || !hex(&meta.user)
        || path.file_name().and_then(|s| s.to_str()) != Some(meta.id.as_str())
        || !valid_name(&meta.name)
        || scope.id(
            &Principal {
                version: 1,
                owner: meta.owner.clone(),
                user: meta.user.clone(),
                channel: "ai".into(),
            },
            &meta.name,
        ) != meta.id
    {
        return Err(Failure::new("browser_profile_unavailable"));
    }
    Ok(meta)
}

impl Profiles {
    fn lease(&self, id: String) -> Result<Lease> {
        if !self.leased.lock().unwrap().insert(id.clone()) {
            return Err(Failure::new("browser_profile_busy"));
        }
        Ok(Lease {
            id,
            leased: self.leased.clone(),
            release: true,
        })
    }
    pub fn open(&self, root: &Path, principal: &Principal, name: &str) -> Result<NamedProfile> {
        self.open_scoped(root, principal, name, Scope::V1)
    }
    pub fn open_scoped(
        &self,
        root: &Path,
        principal: &Principal,
        name: &str,
        scope: Scope,
    ) -> Result<NamedProfile> {
        if !valid_name(name) {
            return Err(Failure::new("browser_invalid_params"));
        }
        let id = scope.id(principal, name);
        let lease = self.lease(id.clone())?;
        let _catalog = self.catalog.lock().unwrap();
        let directory = root.join("profiles");
        private_dir(&directory)?;
        let path = directory.join(&id);
        if path.try_exists().map_err(io_error)? {
            let meta = read(&path, scope)?;
            if meta.owner != principal.owner || meta.user != principal.user || meta.name != name {
                return Err(Failure::new("browser_profile_unavailable"));
            }
            storage::private_metadata(&path.join("data"), true).map_err(io_error)?;
        } else {
            if fs::read_dir(&directory)
                .map_err(io_error)?
                .take(MAX_PROFILES)
                .count()
                >= MAX_PROFILES
            {
                return Err(Failure::new("browser_profile_limit"));
            }
            storage::create_private_dir(&path).map_err(io_error)?;
            let created = (|| {
                storage::create_private_dir(&path.join("data")).map_err(io_error)?;
                storage::write_private_new(
                    &path.join("metadata.json"),
                    &serde_json::to_vec(&Metadata {
                        version: 1,
                        namespace: scope.namespace().map(str::to_owned),
                        id,
                        owner: principal.owner.clone(),
                        user: principal.user.clone(),
                        name: name.into(),
                    })
                    .unwrap(),
                )
                .map_err(io_error)
            })();
            if created.is_err() {
                let _ = fs::remove_dir_all(&path);
            }
            created?;
        }
        Ok(NamedProfile {
            data: path.join("data"),
            lease,
        })
    }
    pub fn list(&self, root: &Path, principal: &Principal) -> Result<Value> {
        self.list_scoped(root, principal, Scope::V1)
    }
    pub fn list_scoped(&self, root: &Path, principal: &Principal, scope: Scope) -> Result<Value> {
        let _catalog = self.catalog.lock().unwrap();
        let directory = root.join("profiles");
        if !directory.try_exists().map_err(io_error)? {
            return Ok(json!({"profiles":[]}));
        }
        storage::private_metadata(&directory, true).map_err(io_error)?;
        let entries = fs::read_dir(&directory)
            .map_err(io_error)?
            .take(MAX_PROFILES + 1)
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(io_error)?;
        if entries.len() > MAX_PROFILES {
            return Err(Failure::new("browser_profile_limit"));
        }
        let mut profiles = Vec::new();
        for entry in entries {
            let meta = read(&entry.path(), scope)?;
            if meta.owner == principal.owner && meta.user == principal.user {
                profiles.push(json!({"profile_id":meta.id,"name":meta.name,
                    "in_use":self.leased.lock().unwrap().contains(&meta.id),"retention":"until_deleted"}));
            }
        }
        profiles.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        if scope == Scope::V1 {
            Ok(json!({"profiles":profiles,"max_profiles_per_device":MAX_PROFILES}))
        } else {
            Ok(json!({"profiles":profiles,"max_profiles_per_namespace":MAX_PROFILES}))
        }
    }
    pub fn delete(&self, root: &Path, principal: &Principal, name: &str) -> Result<Value> {
        self.delete_scoped(root, principal, name, Scope::V1)
    }
    pub fn delete_scoped(
        &self,
        root: &Path,
        principal: &Principal,
        name: &str,
        scope: Scope,
    ) -> Result<Value> {
        let id = scope.id(principal, name);
        let _lease = self.lease(id.clone())?;
        let _catalog = self.catalog.lock().unwrap();
        if !root
            .join("profiles")
            .join(&id)
            .try_exists()
            .map_err(io_error)?
        {
            return Err(Failure::new("browser_profile_unavailable"));
        }
        storage::private_metadata(&root.join("profiles"), true).map_err(io_error)?;
        let path = root.join("profiles").join(&id);
        let meta = read(&path, scope)?;
        if meta.owner != principal.owner || meta.user != principal.user || meta.name != name {
            return Err(Failure::new("browser_profile_unavailable"));
        }
        // A failed recursive removal may already have removed some files.
        fs::remove_dir_all(path).map_err(|_| Failure::unknown("browser_io_error"))?;
        Ok(json!({"profile_id":id,"profile_deleted":true}))
    }
}

#[cfg(test)]
mod scoped_tests {
    use super::*;
    #[test]
    fn named_namespaces_preserve_v1_identity_and_independent_exclusive_leases() {
        let root = std::env::temp_dir().join(format!(
            "rdh-scoped-profiles-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        storage::create_private_dir(&root).unwrap();
        let principal = Principal {
            version: 1,
            owner: "a".repeat(64),
            user: "b".repeat(64),
            channel: "ai".into(),
        };
        let other = Principal {
            owner: "c".repeat(64),
            ..principal.clone()
        };
        let profiles = Profiles::default();
        let mut ids = HashSet::new();
        assert_eq!(Scope::V1.id(&principal, "same"), id(&principal, "same"));
        let mut leases = Vec::new();
        for (directory, scope) in [
            ("v1", Scope::V1),
            ("chrome", Scope::CdpV2),
            ("firefox", Scope::BidiV2),
        ] {
            let path = root.join(directory);
            storage::create_private_dir(&path).unwrap();
            let lease = profiles
                .open_scoped(&path, &principal, "same", scope)
                .unwrap();
            assert!(ids.insert(scope.id(&principal, "same")));
            assert!(
                profiles
                    .open_scoped(&path, &principal, "same", scope)
                    .is_err()
            );
            assert!(
                profiles
                    .delete_scoped(&path, &principal, "same", scope)
                    .is_err()
            );
            assert_eq!(
                profiles.list_scoped(&path, &other, scope).unwrap()["profiles"],
                json!([])
            );
            storage::write_private_new(&lease.data.join("sentinel"), b"persistent").unwrap();
            leases.push((path, scope, lease));
        }
        for (path, scope, lease) in leases {
            drop(lease);
            let reopened = profiles
                .open_scoped(&path, &principal, "same", scope)
                .unwrap();
            assert_eq!(
                storage::read_private(&reopened.data.join("sentinel")).unwrap(),
                b"persistent"
            );
            drop(reopened);
            assert!(
                profiles
                    .list_scoped(
                        &path,
                        &principal,
                        if scope == Scope::V1 {
                            Scope::CdpV2
                        } else {
                            Scope::V1
                        }
                    )
                    .is_err()
            );
            profiles
                .delete_scoped(&path, &principal, "same", scope)
                .unwrap();
            assert_eq!(
                profiles.list_scoped(&path, &principal, scope).unwrap()["profiles"],
                json!([])
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
