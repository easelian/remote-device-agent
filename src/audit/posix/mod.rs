//! POSIX audit filesystem operations. The audit state machine is shared.
use super::implementation::{Failure, Result, fail, synchronize, write_all};
pub(crate) use std::fs::Metadata;
use std::{
    ffi::{CString, OsStr},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileExt, MetadataExt},
        },
    },
    path::{Component, Path, PathBuf},
};
pub(crate) type Identity = (u64, u64);
pub(crate) fn identity(metadata: &Metadata) -> Identity {
    (metadata.dev(), metadata.ino())
}
pub(crate) fn read_at(file: &File, bytes: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(bytes, offset)
}
pub(crate) fn prepare_append(_file: &File) -> Result<()> {
    Ok(())
}

fn os_error() -> Failure {
    io::Error::last_os_error().into()
}
fn cstring(name: &OsStr) -> Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| fail("AUDIT_INVALID_PATH"))
}
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}
pub(crate) fn same_version(left: &Metadata, right: &Metadata) -> bool {
    same_file(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}
fn private(metadata: &Metadata, directory: bool) -> Result<()> {
    // SAFETY: getuid has no arguments or memory side effects.
    let uid = unsafe { libc::getuid() };
    if (if directory {
        !metadata.is_dir()
    } else {
        !metadata.is_file()
    }) || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(fail("AUDIT_UNSAFE_PERMISSIONS"));
    }
    // A hardlink alias could otherwise make retention truncate another
    // private file. Directories legitimately have multiple links.
    if !directory && metadata.nlink() != 1 {
        return Err(fail("AUDIT_HARDLINK_REJECTED"));
    }
    Ok(())
}
fn open_at(parent: &File, name: &OsStr, flags: i32, mode: u32) -> Result<File> {
    let name = cstring(name)?;
    // SAFETY: name is NUL terminated and parent is a live fd. openat returns
    // a fresh owned descriptor, which is constructed only after success.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode,
        )
    };
    if fd < 0 {
        return Err(os_error());
    }
    // SAFETY: fd is uniquely owned from the successful openat above.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn directory(path: &Path, create: bool) -> Result<File> {
    if !path.is_absolute() {
        return Err(fail("AUDIT_INVALID_PATH"));
    }
    let slash = cstring(OsStr::new("/"))?;
    // SAFETY: slash is a valid NUL-terminated path; no fd is borrowed here.
    let fd = unsafe {
        libc::open(
            slash.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(os_error());
    }
    // SAFETY: open succeeded and transferred a unique descriptor.
    let mut current = unsafe { File::from_raw_fd(fd) };
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            _ => return Err(fail("AUDIT_INVALID_PATH")),
        };
        let child = match open_at(&current, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Ok(child) => child,
            Err(error) if error.0 == "ENOENT" && create => {
                let name_c = cstring(name)?;
                // SAFETY: this is a single component under a live directory.
                if unsafe { libc::mkdirat(current.as_raw_fd(), name_c.as_ptr(), 0o700) } < 0
                    && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                {
                    return Err(os_error());
                }
                open_at(&current, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
            }
            Err(error) => return Err(error),
        };
        current = child;
    }
    private(&current.metadata()?, true)?;
    Ok(current)
}

pub(crate) struct Directory {
    path: PathBuf,
    file: File,
}
impl Directory {
    pub(crate) fn open(path: &Path, create: bool) -> Result<Self> {
        Ok(Self {
            path: path.into(),
            file: directory(path, create)?,
        })
    }
    pub(crate) fn synchronize(&self) -> Result<()> {
        synchronize(&self.file).map_err(Failure::from)
    }
    fn verify_directory(&self) -> Result<()> {
        let held = self.file.metadata()?;
        private(&held, true)?;
        let named = directory(&self.path, false)?.metadata()?;
        if !same_file(&held, &named) {
            return Err(fail("AUDIT_DIRECTORY_CHANGED"));
        }
        Ok(())
    }
    pub(crate) fn verify_file(&self, name: &str, file: &File) -> Result<Metadata> {
        self.verify_directory()?;
        let held = file.metadata()?;
        private(&held, false)?;
        let named = open_at(&self.file, OsStr::new(name), libc::O_RDONLY, 0)?.metadata()?;
        private(&named, false)?;
        if !same_file(&held, &named) {
            return Err(fail("AUDIT_FILE_CHANGED"));
        }
        Ok(held)
    }
    pub(crate) fn open_named(&self, name: &str, create: bool) -> Result<File> {
        self.verify_directory()?;
        let file = if create {
            match open_at(
                &self.file,
                OsStr::new(name),
                libc::O_RDWR | libc::O_APPEND | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => {
                    // SAFETY: file is an owned fresh regular-file descriptor.
                    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } < 0 {
                        return Err(os_error());
                    }
                    file
                }
                Err(error) if error.0 == "EEXIST" => open_at(
                    &self.file,
                    OsStr::new(name),
                    libc::O_RDWR | libc::O_APPEND,
                    0,
                )?,
                Err(error) => return Err(error),
            }
        } else {
            open_at(&self.file, OsStr::new(name), libc::O_RDONLY, 0)?
        };
        self.verify_file(name, &file)?;
        Ok(file)
    }
    pub(crate) fn unlink(&self, name: &str, required: bool) -> Result<()> {
        let file = match self.open_named(name, false) {
            Ok(file) => file,
            Err(error) if error.0 == "ENOENT" => return Ok(()),
            Err(error) => return Err(error),
        };
        self.verify_file(name, &file)?;
        let name = cstring(OsStr::new(name))?;
        // SAFETY: name is a single generated component beneath a live fd.
        if unsafe { libc::unlinkat(self.file.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(os_error());
        }
        if required {
            synchronize(&self.file)?;
        }
        Ok(())
    }
    pub(crate) fn rename(&self, source: &str, target: &str, required: bool) -> Result<()> {
        let source_file = self.open_named(source, false)?;
        self.verify_file(source, &source_file)?;
        match self.open_named(target, false) {
            Ok(target_file) => {
                self.verify_file(target, &target_file)?;
            }
            Err(error) if error.0 == "ENOENT" => {}
            Err(error) => return Err(error),
        }
        let source = cstring(OsStr::new(source))?;
        let target = cstring(OsStr::new(target))?;
        // SAFETY: generated single-component names and a live directory fd;
        // rename never follows either leaf symlink.
        if unsafe {
            libc::renameat(
                self.file.as_raw_fd(),
                source.as_ptr(),
                self.file.as_raw_fd(),
                target.as_ptr(),
            )
        } < 0
        {
            return Err(os_error());
        }
        if required {
            synchronize(&self.file)?;
        }
        self.verify_directory()
    }
    pub(crate) fn replace(
        &self,
        name: &str,
        bytes: &[u8],
        expected: &Metadata,
        required: bool,
        fail_before_rename: bool,
    ) -> Result<()> {
        let suffix =
            crate::random::hex_token::<12>().map_err(|_| fail("AUDIT_RANDOM_UNAVAILABLE"))?;
        let temporary_name = format!("{name}.{suffix}.tmp");
        let mut file = open_at(
            &self.file,
            OsStr::new(&temporary_name),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        let temporary = Temporary {
            directory: &self.file,
            name: temporary_name.clone(),
            metadata: file.metadata()?,
        };
        // SAFETY: this is the exclusive new temporary file's descriptor.
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } < 0 {
            return Err(os_error());
        }
        let preparation = (|| -> Result<()> {
            write_all(&mut file, bytes)?;
            if required {
                synchronize(&file)?;
            }
            let named = self.open_named(name, false)?;
            if !same_version(expected, &named.metadata()?) {
                return Err(fail("AUDIT_HISTORY_CHANGED"));
            }
            self.verify_file(&temporary_name, &file)?;
            if fail_before_rename {
                return Err(fail("ENOSPC"));
            }
            self.rename(&temporary_name, name, required)
        })();
        drop(temporary);
        preparation
    }
}
struct Temporary<'a> {
    directory: &'a File,
    name: String,
    metadata: Metadata,
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        // Never unlink an unrelated replacement left at the temporary name.
        let Ok(named) = open_at(self.directory, OsStr::new(&self.name), libc::O_RDONLY, 0) else {
            return;
        };
        let Ok(metadata) = named.metadata() else {
            return;
        };
        if !same_file(&metadata, &self.metadata) {
            return;
        }
        let Ok(name) = cstring(OsStr::new(&self.name)) else {
            return;
        };
        // SAFETY: generated component, live fd, and ownership checked above.
        unsafe {
            libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0);
        }
    }
}
