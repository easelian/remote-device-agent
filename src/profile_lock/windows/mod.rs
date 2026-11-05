//! Windows profile exclusion using Agent-owned lock directories.
//!
//! Recovery requires the same process visibility as the previous owner and
//! native Windows validation.
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::storage::windows::{
    create_child, definitely_not_found, delete_open_object, open_parent,
    open_private_directory_for_deletion, open_private_file_for_deletion, require_child_absent,
};
use crate::{random, storage};

const OWNER_NAME: &[u16] = &[
    b'o' as u16,
    b'w' as u16,
    b'n' as u16,
    b'e' as u16,
    b'r' as u16,
    b'.' as u16,
    b'j' as u16,
    b's' as u16,
    b'o' as u16,
    b'n' as u16,
];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Owner {
    pid: u32,
    started: String,
    nonce: String,
}

fn owner_error() -> Error {
    Error::message(
        "The device configuration is locked or is being recovered. Confirm the previous Agent and its tasks have stopped before retrying.",
    )
}

fn owner_from_bytes(bytes: &[u8]) -> Result<Owner> {
    if bytes.len() > 4096 {
        return Err(owner_error());
    }
    let owner: Owner = serde_json::from_slice(bytes).map_err(|_| owner_error())?;
    if owner.pid == 0
        || owner.started.is_empty()
        || owner.started.len() > 128
        || owner.nonce.encode_utf16().count() < 32
    {
        return Err(owner_error());
    }
    Ok(owner)
}

fn append(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn guard_name(lock_name: &[u16]) -> Result<Vec<u16>> {
    let mut name = lock_name.to_vec();
    name.extend(".reclaiming".encode_utf16());
    if name.len() > 255 {
        return Err(owner_error());
    }
    Ok(name)
}

/// Removes only a directory opened by this process, and only if it is empty.
/// An unsuccessful cleanup leaves a guard or lock behind and therefore fails
/// closed. It never removes a replacement looked up by pathname.
struct NewDirectory(Option<File>);

impl NewDirectory {
    fn create(parent: &File, name: &[u16]) -> Result<Self> {
        create_child(parent, name, true)
            .map(|file| Self(Some(file)))
            .map_err(|error| match &error {
                Error::Io(io) if io.kind() == std::io::ErrorKind::AlreadyExists => owner_error(),
                _ => error,
            })
    }

    fn file(&self) -> &File {
        self.0.as_ref().expect("directory is still held")
    }

    fn keep(&mut self) -> File {
        self.0.take().expect("directory is still held")
    }

    fn remove(&mut self) -> Result<()> {
        if let Some(file) = self.0.as_ref() {
            delete_open_object(file)?;
            drop(self.0.take());
        }
        Ok(())
    }
}

impl Drop for NewDirectory {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

struct HeldLock {
    // Ancestors and the lock directory remain open without delete sharing.
    // This pins the namespace throughout release.
    directories: Vec<File>,
    directory: Option<File>,
    lock_name: Vec<u16>,
    guard_name: Vec<u16>,
    owner: Owner,
}

struct RecoveryGuard {
    directories: Vec<File>,
    lock_name: Vec<u16>,
    guard_name: Vec<u16>,
    guard: NewDirectory,
}

struct StaleLock {
    guard_index: usize,
    directory: File,
    owner_file: File,
    owner: Owner,
}

pub struct ProfileLocks {
    held: Vec<HeldLock>,
    release_on_drop: bool,
}

fn write_owner(directory: &File, owner: &Owner) -> Result<()> {
    let mut file = create_child(directory, OWNER_NAME, false)?;
    let result = (|| {
        serde_json::to_writer(&mut file, owner)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup) = delete_open_object(&file) {
            return Err(Error::message(format!(
                "Writing the lock owner failed ({error}); cleanup also failed ({cleanup})"
            )));
        }
        return Err(error);
    }
    Ok(())
}

impl ProfileLocks {
    pub fn acquire(paths: &[PathBuf]) -> Result<Self> {
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        let mut locks = Self {
            held: Vec::new(),
            release_on_drop: true,
        };
        for config in paths {
            let path = append(&config, ".rdh-lock");
            storage::ensure_parent(&path)?;
            let (directories, name) = open_parent(&path, false, true)?;
            let guard_name = guard_name(&name)?;
            let parent = directories.last().expect("root exists");
            let mut guard = NewDirectory::create(parent, &guard_name)?;
            let mut lock = NewDirectory::create(parent, &name)?;
            let owner = Owner {
                pid: std::process::id(),
                started: crate::process_identity::current()?,
                nonce: random::url_token::<32>()?,
            };
            write_owner(lock.file(), &owner)?;
            locks.held.push(HeldLock {
                directories,
                directory: Some(lock.keep()),
                lock_name: name,
                guard_name,
                owner,
            });
            guard.remove()?;
            let held = locks.held.last().expect("lock was just added");
            require_child_absent(
                held.directories.last().expect("root exists"),
                &held.guard_name,
                true,
            )?;
        }
        Ok(locks)
    }

    pub fn retain_on_drop(&mut self) {
        self.release_on_drop = false;
    }

    pub fn release(&mut self) -> Result<()> {
        for item in self.held.iter_mut().rev() {
            let Some(directory) = item.directory.as_ref() else {
                continue;
            };
            let parent = item.directories.last().expect("root exists");
            let mut guard = NewDirectory::create(parent, &item.guard_name)?;
            let mut owner_file = open_private_file_for_deletion(directory, OWNER_NAME)?;
            let mut bytes = Vec::new();
            (&mut owner_file).take(4097).read_to_end(&mut bytes)?;
            if owner_from_bytes(&bytes)? != item.owner {
                return Err(owner_error());
            }
            delete_open_object(&owner_file)?;
            drop(owner_file);
            require_child_absent(directory, OWNER_NAME, false)?;
            delete_open_object(directory)?;
            drop(item.directory.take());
            require_child_absent(parent, &item.lock_name, true)?;
            guard.remove()?;
            require_child_absent(parent, &item.guard_name, true)?;
        }
        self.held.clear();
        Ok(())
    }

    pub fn recover(paths: &[PathBuf]) -> Result<()> {
        let mut paths = paths.to_vec();
        paths.sort();
        paths.dedup();
        let mut guards = Vec::new();
        let mut deletion_started = false;
        let result = (|| -> Result<()> {
            for config in paths {
                let path = append(&config, ".rdh-lock");
                storage::ensure_parent(&path)?;
                let (directories, lock_name) = open_parent(&path, false, true)?;
                let name = guard_name(&lock_name)?;
                let guard = NewDirectory::create(directories.last().expect("root exists"), &name)?;
                guards.push(RecoveryGuard {
                    directories,
                    lock_name,
                    guard_name: name,
                    guard,
                });
            }
            let mut stale = Vec::new();
            for (index, guard) in guards.iter().enumerate() {
                let parent = guard.directories.last().expect("root exists");
                let directory = match open_private_directory_for_deletion(parent, &guard.lock_name)
                {
                    Ok(directory) => directory,
                    Err(Error::Io(error)) if definitely_not_found(&error) => continue,
                    Err(error) => return Err(error),
                };
                let mut owner_file = open_private_file_for_deletion(&directory, OWNER_NAME)?;
                let mut bytes = Vec::new();
                (&mut owner_file).take(4097).read_to_end(&mut bytes)?;
                let owner = owner_from_bytes(&bytes)?;
                stale.push(StaleLock {
                    guard_index: index,
                    directory,
                    owner_file,
                    owner,
                });
            }
            for lock in &stale {
                if crate::process_identity::running(lock.owner.pid, &lock.owner.started)? {
                    return Err(owner_error());
                }
            }
            // All locks and owner files have been checked before the first delete.
            // Each delete targets the opened object while reclaim guards and
            // ancestor handles remain held; incomplete cleanup fails closed.
            for lock in stale {
                deletion_started = true;
                delete_open_object(&lock.owner_file)?;
                drop(lock.owner_file);
                require_child_absent(&lock.directory, OWNER_NAME, false)?;
                delete_open_object(&lock.directory)?;
                drop(lock.directory);
                let guard = &guards[lock.guard_index];
                require_child_absent(
                    guard.directories.last().expect("root exists"),
                    &guard.lock_name,
                    true,
                )?;
            }
            Ok(())
        })();
        let result = result.map_err(|error| {
            if deletion_started {
                Error::message(format!("Windows lock recovery may be partially complete; inspect all locks before retrying: {error}"))
            } else {
                error
            }
        });
        let mut cleanup_error = None;
        for guard in guards.iter_mut().rev() {
            let outcome = guard.guard.remove().and_then(|()| {
                require_child_absent(
                    guard.directories.last().expect("root exists"),
                    &guard.guard_name,
                    true,
                )
            });
            if let Err(error) = outcome
                && cleanup_error.is_none()
            {
                cleanup_error = Some(error);
            }
        }
        if let Some(cleanup) = cleanup_error {
            return Err(Error::message(match result {
                Ok(()) => format!(
                    "Windows locks may be recovered, but guard cleanup failed; inspect before retrying: {cleanup}"
                ),
                Err(error) => {
                    format!("{error}; Windows recovery guard cleanup also failed: {cleanup}")
                }
            }));
        }
        result
    }
}

impl Drop for ProfileLocks {
    fn drop(&mut self) {
        if self.release_on_drop {
            let _ = self.release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_validation_rejects_unsafe_or_incomplete_records() {
        let nonce = "a".repeat(32);
        let owner = owner_from_bytes(
            format!("{{\"pid\":17,\"started\":\"fixture\",\"nonce\":\"{nonce}\"}}").as_bytes(),
        )
        .unwrap();
        assert_eq!(owner.pid, 17);
        for bad in [
            "{}".to_owned(),
            format!("{{\"pid\":0,\"nonce\":\"{nonce}\"}}"),
            format!("{{\"pid\":4294967296,\"nonce\":\"{nonce}\"}}"),
            "{\"pid\":1,\"nonce\":\"short\"}".to_owned(),
            format!("{{\"pid\":1,\"nonce\":\"{}\"}}", "é".repeat(16)),
            " ".repeat(4097),
        ] {
            assert!(owner_from_bytes(bad.as_bytes()).is_err());
        }
    }
}
