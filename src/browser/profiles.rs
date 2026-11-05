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
fn read(path: &Path) -> Result<Metadata> {
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
    if meta.version != 1
        || !hex(&meta.id)
        || !hex(&meta.owner)
        || !hex(&meta.user)
        || path.file_name().and_then(|s| s.to_str()) != Some(meta.id.as_str())
        || !valid_name(&meta.name)
        || id(
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
        let id = id(principal, name);
        let lease = self.lease(id.clone())?;
        let _catalog = self.catalog.lock().unwrap();
        let directory = root.join("profiles");
        private_dir(&directory)?;
        let path = directory.join(&id);
        if path.try_exists().map_err(io_error)? {
            let meta = read(&path)?;
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
            let meta = read(&entry.path())?;
            if meta.owner == principal.owner && meta.user == principal.user {
                profiles.push(json!({"profile_id":meta.id,"name":meta.name,
                    "in_use":self.leased.lock().unwrap().contains(&meta.id),"retention":"until_deleted"}));
            }
        }
        profiles.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"profiles":profiles,"max_profiles_per_device":MAX_PROFILES}))
    }
    pub fn delete(&self, root: &Path, principal: &Principal, name: &str) -> Result<Value> {
        let id = id(principal, name);
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
        let meta = read(&path)?;
        if meta.owner != principal.owner || meta.user != principal.user || meta.name != name {
            return Err(Failure::new("browser_profile_unavailable"));
        }
        // A failed recursive removal may already have removed some files.
        fs::remove_dir_all(path).map_err(|_| Failure::unknown("browser_io_error"))?;
        Ok(json!({"profile_id":id,"profile_deleted":true}))
    }
}
