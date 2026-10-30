//! Windows profile exclusion using Agent-owned lock directories.
//!
//! Recovery requires the same process visibility as the previous owner and
//! native Windows validation.
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::{ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

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
    nonce: String,
}

fn owner_error() -> Error {
    Error::message(
        "The device configuration is locked or is being recovered. Confirm the previous Agent has stopped before using --recover-lock.",
    )
}

fn owner_from_bytes(bytes: &[u8]) -> Result<Owner> {
    if bytes.len() > 4096 {
        return Err(owner_error());
    }
    let owner: Owner = serde_json::from_slice(bytes).map_err(|_| owner_error())?;
    if owner.pid == 0 || owner.nonce.encode_utf16().count() < 32 {
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

/// Enumerate the complete system process snapshot. An empty or incomplete
/// snapshot is never treated as proof of a dead owner. The caller also checks
/// that this process appears, guarding against a snapshot without process data.
fn process_snapshot() -> Result<Vec<u32>> {
    // SAFETY: no borrowed pointers are supplied; success returns an owned handle.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: a successful snapshot is a unique closeable Windows handle.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: snapshot is live and entry has the required initialized size.
    if unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut processes = Vec::new();
    loop {
        if processes.len() >= 1_000_000 {
            return Err(Error::message(
                "Windows process snapshot exceeds the safety limit",
            ));
        }
        processes.push(entry.th32ProcessID);
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        // SAFETY: snapshot and entry remain valid for the synchronous call.
        if unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
                return Err(error.into());
            }
            break;
        }
    }
    Ok(processes)
}

fn require_absent(processes: &[u32], current: u32, owners: &[u32]) -> Result<()> {
    if current == 0 || !processes.contains(&current) {
        return Err(Error::message(
            "Windows process snapshot cannot confirm its own process",
        ));
    }
    if owners.iter().any(|pid| processes.contains(pid)) {
        return Err(owner_error());
    }
    Ok(())
}

#[cfg(feature = "experimental-windows-install")]
pub(crate) fn require_dead_owners(owners: &[u32]) -> Result<()> {
    require_absent(&process_snapshot()?, std::process::id(), owners)
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
            if !stale.is_empty() {
                let owners: Vec<u32> = stale.iter().map(|lock| lock.owner.pid).collect();
                require_absent(&process_snapshot()?, std::process::id(), &owners)?;
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
        let owner =
            owner_from_bytes(format!("{{\"pid\":17,\"nonce\":\"{nonce}\"}}").as_bytes()).unwrap();
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

    #[test]
    fn an_incomplete_snapshot_or_live_owner_never_authorizes_recovery() {
        assert!(require_absent(&[7, 11], 7, &[23]).is_ok());
        assert!(require_absent(&[7, 11, 23], 7, &[23]).is_err());
        assert!(require_absent(&[11], 7, &[23]).is_err());
        assert!(require_absent(&[], 7, &[23]).is_err());
        assert!(require_absent(&[7], 0, &[23]).is_err());
    }

    #[test]
    fn snapshot_tracks_live_and_exited_child_processes() {
        use std::process::{Child, Command};
        use std::time::{Duration, Instant};

        const CHILD_ENV: &str = "RDH_TEST_PROFILE_SNAPSHOT_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            std::thread::sleep(Duration::from_secs(20));
            return;
        }
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if self.0.try_wait().ok().flatten().is_none() {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "profile_lock::windows::tests::snapshot_tracks_live_and_exited_child_processes",
                ])
                .env(CHILD_ENV, "1")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let expires = Instant::now() + Duration::from_secs(8);
        loop {
            let processes = process_snapshot().unwrap();
            assert!(processes.contains(&std::process::id()));
            if processes.contains(&pid) {
                assert!(require_absent(&processes, std::process::id(), &[pid]).is_err());
                break;
            }
            assert!(
                Instant::now() < expires,
                "child was absent from the process snapshot"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        drop(child);
        let expires = Instant::now() + Duration::from_secs(8);
        loop {
            let processes = process_snapshot().unwrap();
            if !processes.contains(&pid) {
                assert!(require_absent(&processes, std::process::id(), &[pid]).is_ok());
                break;
            }
            assert!(
                Instant::now() < expires,
                "exited child remained in the process snapshot"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
