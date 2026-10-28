//! Windows audit storage beneath retained, private directory handles.
//!
//! Required writes flush the file. Windows directory-entry durability is not
//! promised: this backend does not pretend that directory handles can provide
//! the POSIX directory-fsync contract. The feature remains experimental.
use super::{
    Temporary, check_file_acl, create_child, delete_open_object, failed_publication, file_identity,
    information, named_identity, open_child_request, open_parent, rename_file,
    require_child_absent,
};
use crate::audit::implementation::{Failure, Result, fail};
use crate::error::{Error as StorageError, Result as StorageResult};
use std::{
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    mem::size_of,
    os::windows::{fs::FileExt, io::AsRawHandle},
    path::Path,
};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_BASIC_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
    GetFileInformationByHandleEx, READ_CONTROL, SYNCHRONIZE,
};

pub(crate) type Identity = (u64, [u8; 16]);

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Metadata {
    identity: Identity,
    size: u64,
    modified: i64,
    changed: i64,
}

impl Metadata {
    pub(crate) fn len(&self) -> u64 {
        self.size
    }
}

pub(crate) fn identity(metadata: &Metadata) -> Identity {
    metadata.identity
}

pub(crate) fn same_version(left: &Metadata, right: &Metadata) -> bool {
    left == right
}

fn storage(error: StorageError) -> Failure {
    match error {
        StorageError::Io(error) => error.into(),
        StorageError::Message(message) if message == "AUDIT_HISTORY_CHANGED" => {
            fail("AUDIT_HISTORY_CHANGED")
        }
        StorageError::Message(message)
            if message.starts_with("Private write outcome is unknown") =>
        {
            fail("AUDIT_PUBLICATION_UNKNOWN")
        }
        // Private storage errors may include paths. Never copy them into
        // remotely visible health statistics or audit records.
        _ => fail("AUDIT_UNSAFE_STORAGE"),
    }
}

fn metadata(file: &File) -> StorageResult<Metadata> {
    let info = information(file, false)?;
    check_file_acl(file)?;
    let id = file_identity(file)?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: file is live and basic is aligned, writable output storage.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(Metadata {
        identity: (id.volume, id.file),
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        modified: basic.LastWriteTime,
        changed: basic.ChangeTime,
    })
}

fn component(name: &str) -> Result<Vec<u16>> {
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(fail("AUDIT_INVALID_PATH"));
    }
    Ok(name.encode_utf16().collect())
}

pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(bytes, offset)
}

pub(crate) fn prepare_append(mut file: &File) -> Result<()> {
    // The one audit mutex owns this cursor. Windows needs FILE_WRITE_DATA for
    // rollback truncation, so explicitly seek rather than using an append-only
    // handle which cannot repair a partial record.
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

pub(crate) struct Directory {
    // Ancestors deny FILE_SHARE_DELETE, pinning every component for the entire
    // logger lifetime. None can be redirected by a later pathname lookup.
    directories: Vec<File>,
}

impl Directory {
    pub(crate) fn open(path: &Path, create: bool) -> Result<Self> {
        let (directories, _) =
            open_parent(&path.join("audit-parent-check"), create, true).map_err(storage)?;
        let directory = Self { directories };
        directory.verify()?;
        Ok(directory)
    }

    fn parent(&self) -> &File {
        self.directories.last().expect("retained audit directory")
    }

    fn verify(&self) -> Result<()> {
        information(self.parent(), true).map_err(storage)?;
        check_file_acl(self.parent()).map_err(storage)
    }

    pub(crate) fn synchronize(&self) -> Result<()> {
        // Required record data is flushed by the shared writer. No unchecked
        // or fictitious directory flush is substituted for Windows metadata
        // durability; ACL and handle validity are still checked here.
        self.verify()
    }

    fn open_file(&self, name: &[u16], writable: bool) -> Result<File> {
        let desired = GENERIC_READ
            | READ_CONTROL
            | SYNCHRONIZE
            | if writable { GENERIC_WRITE | DELETE } else { 0 };
        open_child_request(
            self.parent(),
            name,
            false,
            desired,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        )
        .map_err(storage)
    }

    pub(crate) fn verify_file(&self, name: &str, file: &File) -> Result<Metadata> {
        self.verify()?;
        let held = metadata(file).map_err(storage)?;
        let named = named_identity(self.parent(), &component(name)?).map_err(storage)?;
        if named.map(|id| (id.volume, id.file)) != Some(held.identity) {
            return Err(fail("AUDIT_FILE_CHANGED"));
        }
        Ok(held)
    }

    pub(crate) fn open_named(&self, name: &str, create: bool) -> Result<File> {
        self.verify()?;
        let wide = component(name)?;
        let file = match self.open_file(&wide, create) {
            Ok(file) => file,
            Err(error) if create && error.0 == "ENOENT" => {
                let created = match create_child(self.parent(), &wide, false) {
                    Ok(file) => Some(file),
                    Err(StorageError::Io(error))
                        if error.kind() == io::ErrorKind::AlreadyExists =>
                    {
                        None
                    }
                    Err(error) => return Err(storage(error)),
                };
                let expected = created
                    .as_ref()
                    .map(file_identity)
                    .transpose()
                    .map_err(storage)?;
                // Creation is private and exclusive. Reopen with the sharing
                // needed by checked readers/retention, preserving its identity.
                drop(created);
                let file = self.open_file(&wide, true)?;
                if let Some(expected) = expected {
                    if file_identity(&file).map_err(storage)? != expected {
                        return Err(fail("AUDIT_FILE_CHANGED"));
                    }
                }
                file
            }
            Err(error) => return Err(error),
        };
        self.verify_file(name, &file)?;
        Ok(file)
    }

    pub(crate) fn unlink(&self, name: &str, _required: bool) -> Result<()> {
        self.verify()?;
        let wide = component(name)?;
        let file = match self.open_file(&wide, true) {
            Ok(file) => file,
            Err(error) if error.0 == "ENOENT" => return Ok(()),
            Err(error) => return Err(error),
        };
        self.verify_file(name, &file)?;
        delete_open_object(&file).map_err(storage)?;
        drop(file);
        // Do not equate delete-pending with a removed pathname.
        require_child_absent(self.parent(), &wide, false).map_err(storage)
    }

    pub(crate) fn rename(&self, source: &str, target: &str, required: bool) -> Result<()> {
        self.verify()?;
        let source_name = component(source)?;
        let target_name = component(target)?;
        let source_file = self.open_file(&source_name, true)?;
        let source_info = self.verify_file(source, &source_file)?;
        let target_file = match self.open_named(target, false) {
            Ok(file) => Some(file),
            Err(error) if error.0 == "ENOENT" => None,
            Err(error) => return Err(error),
        };
        rename_file(
            &source_file,
            self.parent(),
            &target_name,
            target_file.is_some(),
        )
        .map_err(storage)?;
        if required {
            source_file.sync_all()?;
        }
        let after = self.verify_file(target, &source_file)?;
        if after.identity != source_info.identity {
            return Err(fail("AUDIT_FILE_CHANGED"));
        }
        require_child_absent(self.parent(), &source_name, false).map_err(storage)?;
        self.verify()
    }

    pub(crate) fn replace(
        &self,
        name: &str,
        bytes: &[u8],
        expected: &Metadata,
        required: bool,
        fail_before_rename: bool,
    ) -> Result<()> {
        self.verify()?;
        let target_name = component(name)?;
        let existing = self.open_named(name, false)?;
        if !same_version(expected, &self.verify_file(name, &existing)?) {
            return Err(fail("AUDIT_HISTORY_CHANGED"));
        }
        let temporary_name = component(&format!(
            ".rdh-audit-{}.tmp",
            crate::random::hex_token::<12>().map_err(|_| fail("AUDIT_RANDOM_UNAVAILABLE"))?
        ))?;
        let mut temporary = Temporary {
            file: create_child(self.parent(), &temporary_name, false).map_err(storage)?,
            installed_or_deleted: false,
        };
        let staged = (|| -> StorageResult<()> {
            temporary.file.write_all(bytes)?;
            if required {
                temporary.file.sync_all()?;
            }
            metadata(&temporary.file)?;
            check_file_acl(self.parent())?;
            if !same_version(expected, &metadata(&existing)?)
                || named_identity(self.parent(), &target_name)?.map(|id| (id.volume, id.file))
                    != Some(expected.identity)
            {
                return Err(StorageError::message("AUDIT_HISTORY_CHANGED"));
            }
            if fail_before_rename {
                return Err(io::Error::from(io::ErrorKind::StorageFull).into());
            }
            Ok(())
        })();
        if let Err(error) = staged {
            return failed_publication(temporary, self.parent(), &temporary_name, false, error)
                .map_err(storage);
        }
        temporary.installed_or_deleted = true;
        if let Err(error) = rename_file(&temporary.file, self.parent(), &target_name, true) {
            // The candidate may already have been published. The common private
            // storage cleanup deletes it only when its original name is proven.
            return failed_publication(temporary, self.parent(), &temporary_name, true, error)
                .map_err(storage);
        }
        self.verify_file(name, &temporary.file)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn test_private_directory(path: &Path) {
    use super::private_descriptor;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let descriptor = private_descriptor(true).unwrap();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: path and descriptor remain valid throughout the synchronous call.
    assert_ne!(unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) }, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditLog, AuditRecord};
    use std::{fs, path::PathBuf};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "rdh-windows-audit-{}",
                crate::random::hex_token::<12>().unwrap()
            ));
            test_private_directory(&path);
            Self(path)
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            crate::storage::write_private_new(&self.0.join(name), bytes).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(id: &str) -> AuditRecord {
        AuditRecord {
            id: Some(id.into()),
            operation: "write_file".into(),
            status: "started".into(),
            duration_ms: 0,
            params: serde_json::json!({"path":"sample","content":"AUDIT_PRIVATE_SECRET"}),
            result: None,
            error: None,
        }
    }

    #[test]
    fn inherited_acls_and_hardlinked_history_are_rejected_without_modification() {
        let fixture = Fixture::new();
        let log = fixture.0.join("device.jsonl");
        // A newly inherited ACL is deliberately not the protected ACL required
        // by private storage; initialization must not silently repair it.
        fs::write(&log, b"original\n").unwrap();
        assert!(AuditLog::open(&fixture.0, "device", 4096, 3, true).is_err());
        assert_eq!(fs::read(&log).unwrap(), b"original\n");
        fs::remove_file(&log).unwrap();
        fixture.write("target", b"preserved\n");
        fs::hard_link(fixture.0.join("target"), &log).unwrap();
        assert!(AuditLog::open(&fixture.0, "device", 4096, 3, true).is_err());
        assert_eq!(fs::read(&log).unwrap(), b"preserved\n");
        assert_eq!(fs::read(fixture.0.join("target")).unwrap(), b"preserved\n");
    }

    #[test]
    fn retained_directory_and_changed_log_identity_reject_redirected_writes() {
        let fixture = Fixture::new();
        let log = AuditLog::open(&fixture.0, "device", 4096, 3, true).unwrap();
        log.record(record("before")).unwrap();
        let before = fs::read(fixture.0.join("device.jsonl")).unwrap();
        let moved = fixture.0.with_extension("moved");
        if fs::rename(&fixture.0, &moved).is_ok() {
            drop(log);
            fs::rename(&moved, &fixture.0).unwrap();
            panic!("a retained Windows audit directory was renamed");
        }
        fs::rename(
            fixture.0.join("device.jsonl"),
            fixture.0.join("saved.jsonl"),
        )
        .unwrap();
        fixture.write("device.jsonl", b"");
        assert!(log.record(record("redirected")).is_err());
        assert!(fs::read(fixture.0.join("device.jsonl")).unwrap().is_empty());
        assert_eq!(fs::read(fixture.0.join("saved.jsonl")).unwrap(), before);
        assert_eq!(log.stats()["dropped"], 1);
        assert_eq!(log.stats()["last_error"], "AUDIT_FILE_CHANGED");
    }

    #[test]
    fn pending_history_deletion_is_reported_until_the_reader_closes() {
        let fixture = Fixture::new();
        fixture.write("device.jsonl.1", b"history\n");
        let directory = Directory::open(&fixture.0, false).unwrap();
        let reader = directory.open_named("device.jsonl.1", false).unwrap();
        assert!(directory.unlink("device.jsonl.1", true).is_err());
        drop(reader);
        require_child_absent(
            directory.parent(),
            &component("device.jsonl.1").unwrap(),
            false,
        )
        .unwrap();
    }
}
