//! Directory-relative POSIX operations for the shared file protocol.
use super::*;
use std::{
    ffi::{CStr, CString},
    fs::{File, Metadata},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::fs::MetadataExt,
    },
};

pub(super) type Opened = File;

pub(super) fn metadata(file: &Opened) -> Result<MetadataFacts, String> {
    file.metadata()
        .map(|value| MetadataFacts::from_metadata(&value))
        .map_err(io_error)
}

pub(in crate::files) struct Root {
    file: File,
    path: String,
}

fn cstring(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "Path must contain no NUL bytes".into())
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn open_beneath(parent: &File, path: &str, flags: i32) -> Result<File, String> {
    open_beneath_mode(parent, path, flags, 0)
}

#[cfg(target_os = "linux")]
fn open_beneath_mode(parent: &File, path: &str, flags: i32, mode: u32) -> Result<File, String> {
    let name = cstring(path)?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: u64::from(mode),
        resolve: 0x08 | 0x04 | 0x02,
    };
    // SAFETY: name/how remain alive through the syscall. Success returns a
    // fresh owned descriptor; no descriptor is constructed on failure.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
            return Err("Secure file operations require kernel openat2 support".into());
        }
        return Err(io_error(error));
    }
    // SAFETY: this descriptor was returned uniquely by openat2 above.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

#[cfg(target_os = "macos")]
fn open_beneath_mode(parent: &File, path: &str, flags: i32, mode: u32) -> Result<File, String> {
    super::super::macos::open_beneath(parent, &normalize(path)?, flags, mode).map_err(io_error)
}

fn absolute_root(path: &str) -> Result<File, String> {
    if !path.starts_with('/') || path.encode_utf16().count() > 4096 {
        return Err("Root must be an absolute directory path".into());
    }
    #[cfg(target_os = "macos")]
    {
        super::super::macos::check_capabilities().map_err(io_error)?;
        if std::fs::canonicalize(path).map_err(io_error)? != std::path::Path::new(path) {
            return Err("Root must be a canonical directory".into());
        }
    }
    let slash = cstring("/")?;
    // SAFETY: slash is NUL terminated; open returns a fresh descriptor.
    let fd = unsafe {
        libc::open(
            slash.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io_error(std::io::Error::last_os_error()));
    }
    // SAFETY: ownership of fd is transferred exactly once.
    let root = unsafe { File::from_raw_fd(fd) };
    let relative = normalize(path.trim_start_matches('/'))?;
    open_beneath(&root, &relative, libc::O_RDONLY | libc::O_DIRECTORY)
}

impl Root {
    pub(in crate::files) fn new(path: &str) -> Result<Self, String> {
        Ok(Self {
            file: absolute_root(path)?,
            path: path.into(),
        })
    }
    pub(super) fn verify(&self) -> Result<(), String> {
        let named = absolute_root(&self.path)?;
        let a = self.file.metadata().map_err(io_error)?;
        let b = named.metadata().map_err(io_error)?;
        if a.dev() != b.dev() || a.ino() != b.ino() {
            return Err("Configured root directory changed".into());
        }
        Ok(())
    }
    pub(super) fn open(&self, path: &str, directory: bool) -> Result<File, String> {
        self.verify()?;
        open_beneath(
            &self.file,
            &normalize(path)?,
            libc::O_RDONLY | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        )
    }
    pub(super) fn metadata(&self, path: &str) -> Result<MetadataFacts, String> {
        self.verify()?;
        let path = normalize(path)?;
        #[cfg(target_os = "linux")]
        {
            // Metadata-only descriptors do not activate special devices.
            let metadata = open_beneath(&self.file, &path, libc::O_PATH)?
                .metadata()
                .map_err(io_error)?;
            Ok(MetadataFacts::from_metadata(&metadata))
        }
        #[cfg(target_os = "macos")]
        {
            if path == "." {
                return Ok(MetadataFacts::from_metadata(
                    &self.file.metadata().map_err(io_error)?,
                ));
            }
            let (relative, name) = path.rsplit_once('/').unwrap_or((".", &path));
            let parent = Parent::directory(self, relative)?;
            parent.verify()?;
            let stat =
                super::super::macos::metadata_at(&parent.directory, name).map_err(io_error)?;
            parent.verify()?;
            // fstatat never opens or follows the leaf, including devices,
            // FIFOs and symlinks. Callers accept only regular files/dirs.
            Ok(MetadataFacts::from_stat(&stat))
        }
    }
}

fn mtime(m: &Metadata) -> f64 {
    m.mtime() as f64 * 1000.0 + m.mtime_nsec() as f64 / 1_000_000.0
}

impl MetadataFacts {
    fn from_metadata(m: &Metadata) -> Self {
        Self {
            identity: FileIdentity {
                volume: m.dev(),
                id: u128::from(m.ino()),
            },
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mode: m.mode(),
            modified_ms: mtime(m),
            is_file: m.is_file(),
            is_directory: m.is_dir(),
        }
    }
    #[cfg(target_os = "macos")]
    fn from_stat(m: &libc::stat) -> Self {
        let mode = u32::from(m.st_mode);
        Self {
            identity: FileIdentity {
                volume: m.st_dev as u64,
                id: u128::from(m.st_ino),
            },
            dev: m.st_dev as u64,
            ino: m.st_ino,
            size: m.st_size.max(0) as u64,
            mode,
            modified_ms: m.st_mtime as f64 * 1000.0 + m.st_mtime_nsec as f64 / 1_000_000.0,
            is_file: m.st_mode & libc::S_IFMT == libc::S_IFREG,
            is_directory: m.st_mode & libc::S_IFMT == libc::S_IFDIR,
        }
    }
}

struct Dir(*mut libc::DIR);
impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: fdopendir allocated this DIR and this owner closes it once.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
pub(super) fn entries_bounded(
    file: File,
    limit: usize,
    control: Option<(&std::sync::atomic::AtomicBool, Instant)>,
) -> Result<(Vec<Entry>, bool), String> {
    let fd = file.into_raw_fd();
    // SAFETY: fd is an owned directory descriptor, transferred on success.
    let pointer = unsafe { libc::fdopendir(fd) };
    if pointer.is_null() {
        let error = std::io::Error::last_os_error();
        // SAFETY: fdopendir failed, leaving fd owned by this function.
        unsafe {
            libc::close(fd);
        }
        return Err(io_error(error));
    }
    let dir = Dir(pointer);
    let mut result = Vec::new();
    loop {
        if control.is_some_and(|(cancelled, deadline)| {
            cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline
        }) {
            return Err("Search cancelled or expired".into());
        }
        // SAFETY: errno is thread-local, dir is live and used by one caller.
        unsafe {
            #[cfg(target_os = "linux")]
            {
                *libc::__errno_location() = 0;
            }
            #[cfg(target_os = "macos")]
            {
                *libc::__error() = 0;
            }
        }
        let entry = unsafe { libc::readdir(dir.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(io_error(error));
            }
            break;
        }
        // SAFETY: readdir provides a NUL-terminated d_name valid until the
        // next call; name is copied before that call.
        let raw = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name = raw
            .to_str()
            .map_err(|_| "Directory contains a filename not representable as UTF-8".to_string())?;
        if name == "." || name == ".." {
            continue;
        }
        if result.len() >= limit {
            return Ok((result, true));
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: raw is valid and stat points to sufficient uninitialized
        // storage. AT_SYMLINK_NOFOLLOW never dereferences a symlink entry.
        if unsafe {
            libc::fstatat(
                fd,
                raw.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        // SAFETY: successful fstatat initialized stat.
        let stat = unsafe { stat.assume_init() };
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => "file",
            libc::S_IFDIR => "directory",
            libc::S_IFLNK => "symlink",
            _ => "other",
        };
        result.push(Entry {
            name: name.into(),
            kind,
            size: stat.st_size.max(0) as u64,
        });
    }
    Ok((result, false))
}

/// A directory capability plus its rooted name. Verification detects stale
/// names before and after mutation. It cannot make the check and renameat /
/// mkdirat commit one atomic operation against external directory renames.
pub(super) struct Parent<'a> {
    root: &'a Root,
    relative: String,
    directory: File,
}
impl<'a> Parent<'a> {
    fn directory(root: &'a Root, relative: &str) -> Result<Self, String> {
        let relative = normalize(relative)?;
        let directory = root.open(&relative, true)?;
        Ok(Self {
            root,
            relative,
            directory,
        })
    }
    pub(super) fn file(root: &'a Root, path: &str, create: bool) -> Result<(Self, String), String> {
        let path = normalize(path)?;
        if path == "." {
            return Err("A file name is required".into());
        }
        let (relative, name) = path.rsplit_once('/').unwrap_or((".", &path));
        if create {
            make_directories(root, relative)?;
        }
        Ok((Self::directory(root, relative)?, name.into()))
    }
    fn verify(&self) -> Result<(), String> {
        let named = self
            .root
            .open(&self.relative, true)?
            .metadata()
            .map_err(io_error)?;
        let held = self.directory.metadata().map_err(io_error)?;
        if held.dev() != named.dev() || held.ino() != named.ino() {
            return Err("Directory path changed while performing file operation".into());
        }
        Ok(())
    }
}

fn stat_at(directory: &File, name: &str) -> Result<Option<libc::stat>, String> {
    let name = cstring(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: directory is live, name is NUL terminated and stat has the
    // required size. NOFOLLOW observes symlinks without dereferencing them.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(io_error(error));
    }
    // SAFETY: successful fstatat initialized every returned stat field.
    Ok(Some(unsafe { stat.assume_init() }))
}
fn same_inode(stat: &libc::stat, metadata: &Metadata) -> bool {
    stat.st_dev == metadata.dev() as libc::dev_t && stat.st_ino == metadata.ino() as libc::ino_t
}
fn same_version(stat: &libc::stat, metadata: &Metadata) -> bool {
    same_inode(stat, metadata)
        && stat.st_size >= 0
        && stat.st_size as u64 == metadata.len()
        && stat.st_mode == metadata.mode() as libc::mode_t
        && stat.st_mtime == metadata.mtime()
        && stat.st_mtime_nsec == metadata.mtime_nsec()
        && stat.st_ctime == metadata.ctime()
        && stat.st_ctime_nsec == metadata.ctime_nsec()
}
fn verify_file(parent: &Parent<'_>, name: &str, metadata: &Metadata) -> Result<(), String> {
    parent.verify()?;
    let named =
        stat_at(&parent.directory, name)?.ok_or("File changed while performing file operation")?;
    if !same_version(&named, metadata) {
        return Err("File changed while performing file operation".into());
    }
    Ok(())
}
pub(super) fn make_directories(root: &Root, path: &str) -> Result<(), String> {
    let path = normalize(path)?;
    let components: Vec<&str> = if path == "." {
        Vec::new()
    } else {
        path.split('/').collect()
    };
    if components.len() > 128 {
        return Err("Directory creation exceeds 128 path components".into());
    }
    let mut prefix = String::from(".");
    for component in components {
        let parent = Parent::directory(root, &prefix)?;
        parent.verify()?;
        if stat_at(&parent.directory, component)?.is_none() {
            let name = cstring(component)?;
            // SAFETY: this is a single validated component beneath a live
            // directory descriptor. mkdirat does not follow the leaf.
            if unsafe { libc::mkdirat(parent.directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) {
                    return Err(io_error(error));
                }
            }
            crate::storage::sync_directory(&parent.directory).map_err(io_error)?;
        }
        parent.verify()?;
        prefix = if prefix == "." {
            component.into()
        } else {
            format!("{prefix}/{component}")
        };
        // Validate existing and concurrently created entries through the
        // root. A symlink or non-directory always fails closed here.
        drop(root.open(&prefix, true)?);
    }
    root.verify()
}

pub(super) struct Existing {
    pub(super) bytes: Vec<u8>,
    metadata: Metadata,
}
pub(super) fn read_existing(parent: &Parent<'_>, name: &str) -> Result<Option<Existing>, String> {
    parent.verify()?;
    let Some(named) = stat_at(&parent.directory, name)? else {
        return Ok(None);
    };
    if named.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err("Only regular files are supported".into());
    }
    let mut file = open_beneath(&parent.directory, name, libc::O_RDONLY | libc::O_NONBLOCK)?;
    regular(&file, MAX_FILE as u64)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !same_version(&named, &metadata) {
        return Err("File changed while opening".into());
    }
    let bytes = read_full(&mut file)?;
    verify_file(parent, name, &metadata)?;
    Ok(Some(Existing { bytes, metadata }))
}

fn random_temp_name() -> Result<String, String> {
    crate::random::hex_token::<16>()
        .map(|suffix| format!(".rdh-{suffix}"))
        .map_err(|_| "Secure randomness is unavailable".into())
}

struct Temporary<'a> {
    directory: &'a File,
    name: String,
    file: File,
    present: bool,
}
impl<'a> Temporary<'a> {
    fn create(directory: &'a File) -> Result<Self, String> {
        let name = random_temp_name()?;
        let file = open_beneath_mode(
            directory,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        Ok(Self {
            directory,
            name,
            file,
            present: true,
        })
    }
    fn verify(&self) -> Result<(), String> {
        let named = stat_at(self.directory, &self.name)?.ok_or("Temporary file disappeared")?;
        let held = self.file.metadata().map_err(io_error)?;
        if named.st_mode & libc::S_IFMT != libc::S_IFREG || !same_version(&named, &held) {
            return Err("Temporary file changed".into());
        }
        Ok(())
    }
    fn remove(&mut self) -> Result<(), String> {
        if !self.present {
            return Ok(());
        }
        self.verify()?;
        let name = cstring(&self.name)?;
        // SAFETY: name is a single component; the owned directory fd is
        // live. unlinkat removes this name and never follows a symlink.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        self.present = false;
        Ok(())
    }
}
impl Drop for Temporary<'_> {
    fn drop(&mut self) {
        // Errors on the normal path are returned explicitly. This is only
        // best-effort cleanup while propagating an already-uncertain error.
        let _ = self.remove();
    }
}

pub(super) fn write_atomic(
    parent: &Parent<'_>,
    name: &str,
    bytes: &[u8],
    existing: Option<&Existing>,
) -> Result<(), String> {
    parent.verify()?;
    let mut temporary = Temporary::create(&parent.directory)?;
    let result = (|| -> Result<(), String> {
        // The temporary starts private. Only fully written content receives
        // the target's permissions, before sync and the atomic commit.
        temporary.file.write_all(bytes).map_err(io_error)?;
        let mode = existing.map_or(0o600, |previous| previous.metadata.mode() & 0o777);
        // SAFETY: fchmod operates on our live, exclusively created file.
        if unsafe { libc::fchmod(temporary.file.as_raw_fd(), mode as libc::mode_t) } < 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        temporary.file.sync_all().map_err(io_error)?;
        parent.verify()?;
        temporary.verify()?;
        if let Some(previous) = existing {
            // This detects observed external changes; it is not an atomic
            // cross-process compare-and-swap against the final rename.
            verify_file(parent, name, &previous.metadata)?;
        } else if stat_at(&parent.directory, name)?.is_some() {
            return Err("File already exists".into());
        }
        let source = cstring(&temporary.name)?;
        let target = cstring(name)?;
        if existing.is_some() {
            // SAFETY: both names are single components in the held parent.
            // renameat atomically replaces the name without following it.
            if unsafe {
                libc::renameat(
                    parent.directory.as_raw_fd(),
                    source.as_ptr(),
                    parent.directory.as_raw_fd(),
                    target.as_ptr(),
                )
            } < 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            temporary.present = false;
        } else {
            // SAFETY: linkat with no FOLLOW flag links the temporary name,
            // and fails with EEXIST instead of overwriting the destination.
            if unsafe {
                libc::linkat(
                    parent.directory.as_raw_fd(),
                    source.as_ptr(),
                    parent.directory.as_raw_fd(),
                    target.as_ptr(),
                    0,
                )
            } < 0
            {
                return Err(io_error(std::io::Error::last_os_error()));
            }
            temporary.remove()?;
        }
        crate::storage::sync_directory(&parent.directory).map_err(io_error)?;
        parent.verify()?;
        let committed = stat_at(&parent.directory, name)?.ok_or("Committed file disappeared")?;
        if !same_inode(&committed, &temporary.file.metadata().map_err(io_error)?) {
            return Err("Committed file changed".into());
        }
        Ok(())
    })();
    let cleanup = temporary.remove();
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

pub(super) fn move_file(root: &Root, source: &str, destination: &str) -> Result<(), String> {
    let (source_parent, source_name) = Parent::file(root, source, false)?;
    let (destination_parent, destination_name) = Parent::file(root, destination, false)?;
    let source_file = open_beneath(
        &source_parent.directory,
        &source_name,
        libc::O_RDONLY | libc::O_NONBLOCK,
    )?;
    let source_metadata = source_file.metadata().map_err(io_error)?;
    if !source_metadata.is_file() {
        return Err("Only regular files can be moved".into());
    }
    verify_file(&source_parent, &source_name, &source_metadata)?;
    destination_parent.verify()?;
    if stat_at(&destination_parent.directory, &destination_name)?.is_some() {
        return Err("Destination already exists".into());
    }
    if source_metadata.dev()
        != destination_parent
            .directory
            .metadata()
            .map_err(io_error)?
            .dev()
    {
        return Err("Moving files across filesystems is disabled".into());
    }
    let source_c = cstring(&source_name)?;
    let destination_c = cstring(&destination_name)?;
    // SAFETY: both directory descriptors are live and each string is a
    // single component. NOREPLACE atomically rejects any destination;
    // rename does not follow either leaf. There is no weaker fallback.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.directory.as_raw_fd(),
            source_c.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            source_parent.directory.as_raw_fd(),
            source_c.as_ptr(),
            destination_parent.directory.as_raw_fd(),
            destination_c.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EXDEV) {
            return Err("Moving files across filesystems is disabled".into());
        }
        if error.raw_os_error() == Some(libc::EEXIST) {
            return Err("Destination already exists".into());
        }
        return Err(io_error(error));
    }
    crate::storage::sync_directory(&source_parent.directory).map_err(io_error)?;
    crate::storage::sync_directory(&destination_parent.directory).map_err(io_error)?;
    source_parent.verify()?;
    destination_parent.verify()?;
    let moved = stat_at(&destination_parent.directory, &destination_name)?
        .ok_or("Moved file disappeared")?;
    if moved.st_mode & libc::S_IFMT != libc::S_IFREG || !same_inode(&moved, &source_metadata) {
        return Err("Moved file changed".into());
    }
    Ok(())
}
