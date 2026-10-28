//! Darwin directory-relative opens. Never fall back when NOFOLLOW_ANY is absent.
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::OnceLock;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn name(value: &str) -> io::Result<CString> {
    CString::new(value).map_err(|_| invalid("Invalid relative file path"))
}
fn raw_open(parent: &File, path: &str, flags: i32, mode: u32) -> io::Result<File> {
    let path = name(path)?;
    // Darwin rejects O_NOFOLLOW together with O_NOFOLLOW_ANY. The latter
    // covers the final component as well as every intermediate component.
    let flags =
        (flags & !libc::O_NOFOLLOW) | libc::O_CLOEXEC | libc::O_NONBLOCK | libc::O_NOFOLLOW_ANY;
    // SAFETY: parent owns a live fd, path is NUL terminated and the creation
    // mode is promoted for Darwin's variadic openat argument.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            path.as_ptr(),
            flags,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned this uniquely owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn descriptor_path(file: &File) -> io::Result<PathBuf> {
    let mut buffer = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH requires a MAXPATHLEN buffer, kept writable and live.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let path = CStr::from_bytes_until_nul(&buffer)
        .map_err(|_| invalid("Descriptor path is unterminated"))?;
    let path = PathBuf::from(OsStr::from_bytes(path.to_bytes()));
    if !path.is_absolute() {
        return Err(invalid("Descriptor path is not absolute"));
    }
    Ok(path)
}
fn verify_beneath(parent: &File, file: &File) -> io::Result<()> {
    let base = descriptor_path(parent)?;
    let target = descriptor_path(file)?;
    if !target.starts_with(&base) {
        return Err(invalid("Opened file escaped its directory"));
    }
    Ok(())
}

fn stat_at(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut value = std::mem::MaybeUninit::uninit();
    // SAFETY: both borrowed arguments remain live and stat has sufficient size.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized the result.
    Ok(unsafe { value.assume_init() })
}
fn same(stat: &libc::stat, metadata: &Metadata) -> bool {
    stat.st_dev as u64 == metadata.dev() && stat.st_ino == metadata.ino()
}

pub(super) fn metadata_at(parent: &File, leaf: &str) -> io::Result<libc::stat> {
    let stat = stat_at(parent, &name(leaf)?)?;
    let kind = stat.st_mode & libc::S_IFMT;
    if !matches!(kind, libc::S_IFREG | libc::S_IFDIR) {
        return Err(invalid("Only regular files and directories are supported"));
    }
    {
        // Descriptor devices must never turn a named device node into a file
        // capability for another directory, even on a virtual getattr path.
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        // SAFETY: parent is live and the output has the required native size.
        if unsafe { libc::fstatfs(parent.as_raw_fd(), filesystem.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the result was zero-initialized and fstatfs succeeded.
        let filesystem = unsafe { filesystem.assume_init() };
        // SAFETY: the initialized char array is borrowed as bytes of equal size.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                filesystem.f_fstypename.as_ptr().cast::<u8>(),
                filesystem.f_fstypename.len(),
            )
        };
        let filesystem =
            CStr::from_bytes_until_nul(bytes).map_err(|_| invalid("Filesystem name is invalid"))?;
        let filesystem = filesystem.to_bytes();
        let descriptor_entry = leaf != "."
            && (matches!(filesystem, b"fdesc" | b"fdescfs")
                || (filesystem == b"devfs"
                    && descriptor_path(parent)?.file_name() == Some(OsStr::new("fd"))));
        if descriptor_entry || (kind == libc::S_IFREG && filesystem == b"devfs") {
            return Err(invalid("Descriptor device aliases are unsupported"));
        }
    }
    Ok(stat)
}

fn verify_named(parent: &File, leaf: &str, file: &File) -> io::Result<()> {
    let named = metadata_at(parent, leaf)?;
    let held = file.metadata()?;
    if !same(&named, &held)
        || u32::from(named.st_mode) & u32::from(libc::S_IFMT)
            != held.mode() & u32::from(libc::S_IFMT)
    {
        return Err(invalid("File changed while opening"));
    }
    Ok(())
}
fn remove_created(parent: &File, name: &str, file: &File) {
    let Ok(name) = self::name(name) else {
        return;
    };
    let (Ok(named), Ok(held)) = (stat_at(parent, &name), file.metadata()) else {
        return;
    };
    if named.st_mode & libc::S_IFMT == libc::S_IFREG && same(&named, &held) {
        // SAFETY: only this exclusively created, still-matching leaf is removed.
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0);
        }
    }
}

fn open_directory(parent: &File, path: &str) -> io::Result<File> {
    let mut directory = raw_open(parent, ".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    for component in path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        let named = metadata_at(&directory, component)?;
        if named.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(invalid("File parent is not a directory"));
        }
        let next = raw_open(&directory, component, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        verify_named(&directory, component, &next)?;
        directory = next;
    }
    verify_beneath(parent, &directory)?;
    Ok(directory)
}

pub(super) fn open_beneath(parent: &File, path: &str, flags: i32, mode: u32) -> io::Result<File> {
    if path.starts_with('/')
        || path.split('/').any(|part| part == "..")
        || path.encode_utf16().count() > 4096
    {
        return Err(invalid("Invalid relative file path"));
    }
    let creating = flags & libc::O_CREAT != 0;
    if creating && flags & libc::O_EXCL == 0 {
        return Err(invalid("File creation must be exclusive"));
    }
    let (directory, leaf) = path.rsplit_once('/').unwrap_or((".", path));
    // Every intermediate component must be an actual directory, even when
    // Darwin recognizes a virtual resource-fork suffix after a regular file.
    let directory = open_directory(parent, directory)?;
    if !directory.metadata()?.is_dir() {
        return Err(invalid("File parent is not a directory"));
    }
    verify_beneath(parent, &directory)?;
    if !creating {
        metadata_at(&directory, leaf)?;
    }
    let file = raw_open(&directory, leaf, flags, mode)?;
    let verified = (|| {
        // Regular-file hardlinks are authorized by the selected name. F_GETPATH
        // may select another alias, so use it only for directory containment.
        verify_beneath(parent, &directory)?;
        verify_named(&directory, leaf, &file)?;
        if file.metadata()?.is_dir() {
            verify_beneath(parent, &file)?;
        }
        Ok(())
    })();
    if let Err(error) = verified {
        if creating {
            remove_created(&directory, leaf, &file);
        }
        return Err(error);
    }
    Ok(file)
}

struct ProbeEntry {
    parent: File,
    name: CString,
    identity: (u64, u64),
    directory: bool,
    present: bool,
}
impl ProbeEntry {
    fn remember(parent: &File, leaf: &str, directory: bool) -> io::Result<Self> {
        let name = name(leaf)?;
        let stat = stat_at(parent, &name)?;
        let kind = stat.st_mode & libc::S_IFMT;
        if stat.st_uid != unsafe { libc::getuid() }
            || (directory && kind != libc::S_IFDIR)
            || (!directory && !matches!(kind, libc::S_IFREG | libc::S_IFLNK))
        {
            return Err(invalid("Probe entry changed"));
        }
        Ok(Self {
            parent: parent.try_clone()?,
            name,
            identity: (stat.st_dev as u64, stat.st_ino),
            directory,
            present: true,
        })
    }
    fn remove(&mut self) -> io::Result<()> {
        if !self.present {
            return Ok(());
        }
        let stat = match stat_at(&self.parent, &self.name) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.present = false;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if (stat.st_dev as u64, stat.st_ino) != self.identity {
            return Err(invalid("Probe entry changed"));
        }
        // SAFETY: the name still identifies the recorded entry beneath its fd.
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                if self.directory {
                    libc::AT_REMOVEDIR
                } else {
                    0
                },
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        self.present = false;
        Ok(())
    }
}
impl Drop for ProbeEntry {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}
struct Probe {
    path: PathBuf,
    directory: File,
    entries: Vec<ProbeEntry>,
    present: bool,
}
impl Probe {
    fn new() -> io::Result<Self> {
        let suffix = crate::random::hex_token::<16>()
            .map_err(|_| invalid("Probe randomness is unavailable"))?;
        let path = std::env::temp_dir()
            .canonicalize()?
            .join(format!("rdh-file-capability-{suffix}"));
        DirBuilder::new().mode(0o700).create(&path)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        let probe = Self {
            path,
            directory,
            entries: Vec::new(),
            present: true,
        };
        let metadata = probe.directory.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(invalid("Unsafe file capability probe directory"));
        }
        Ok(probe)
    }
    fn remove(&mut self) -> io::Result<()> {
        let mut failure = None;
        for entry in self.entries.iter_mut().rev() {
            if let Err(error) = entry.remove() {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if self.present {
            let held = self.directory.metadata()?;
            let named = fs::symlink_metadata(&self.path)?;
            if !named.is_dir() || named.dev() != held.dev() || named.ino() != held.ino() {
                return Err(invalid("File capability probe directory changed"));
            }
            fs::remove_dir(&self.path)?;
            self.present = false;
        }
        Ok(())
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

fn probe_capabilities() -> io::Result<()> {
    let mut probe = Probe::new()?;
    let real = name("real")?;
    // SAFETY: a fixed new component beneath the private probe directory.
    if unsafe { libc::mkdirat(probe.directory.as_raw_fd(), real.as_ptr(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    probe
        .entries
        .push(ProbeEntry::remember(&probe.directory, "real", true)?);
    let directory = raw_open(
        &probe.directory,
        "real",
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    let mut file = raw_open(
        &directory,
        "file",
        libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
        0o600,
    )?;
    probe
        .entries
        .push(ProbeEntry::remember(&directory, "file", false)?);
    file.write_all(b"probe")?;
    for (target, leaf) in [("real", "link"), ("real/file", "leaf")] {
        let target = name(target)?;
        let leaf_c = name(leaf)?;
        // SAFETY: both fixed names point only within this owned probe.
        if unsafe {
            libc::symlinkat(
                target.as_ptr(),
                probe.directory.as_raw_fd(),
                leaf_c.as_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        probe
            .entries
            .push(ProbeEntry::remember(&probe.directory, leaf, false)?);
    }
    verify_beneath(&probe.directory, &directory)?;
    // Check callers with and without an explicit final-component flag. Both
    // must open a regular path and reject links anywhere in the path.
    for flags in [libc::O_RDONLY, libc::O_RDONLY | libc::O_NOFOLLOW] {
        let positive = raw_open(&probe.directory, "real/file", flags, 0)?;
        verify_named(&directory, "file", &positive)?;
        for path in ["link/file", "leaf"] {
            match raw_open(&probe.directory, path, flags, 0) {
                Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {}
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Kernel does not confirm NOFOLLOW_ANY file protection",
                    ));
                }
            }
        }
    }
    drop(file);
    drop(directory);
    probe.remove()
}
pub(super) fn check_capabilities() -> io::Result<()> {
    static CHECKED: OnceLock<Result<(), String>> = OnceLock::new();
    CHECKED
        .get_or_init(|| {
            probe_capabilities()
                .map_err(|error| format!("Secure macOS file operations are unavailable ({error})"))
        })
        .clone()
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_capabilities_reject_leaf_and_parent_links() {
        probe_capabilities().unwrap();
    }

    #[test]
    fn descriptor_containment_rejects_a_directory_moved_outside_its_root() {
        check_capabilities().unwrap();
        let mut root = Probe::new().unwrap();
        let mut outside = Probe::new().unwrap();
        let inside = root.path.join("inside");
        DirBuilder::new().mode(0o700).create(&inside).unwrap();
        root.entries
            .push(ProbeEntry::remember(&root.directory, "inside", true).unwrap());
        let held = raw_open(
            &root.directory,
            "inside",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .unwrap();
        verify_beneath(&root.directory, &held).unwrap();
        let moved = outside.path.join("moved");
        fs::rename(&inside, &moved).unwrap();
        assert!(verify_beneath(&root.directory, &held).is_err());
        fs::rename(moved, &inside).unwrap();
        verify_beneath(&root.directory, &held).unwrap();
        drop(held);
        root.remove().unwrap();
        outside.remove().unwrap();
    }

    #[test]
    fn descriptor_devices_do_not_expose_an_open_file_outside_the_root() {
        check_capabilities().unwrap();
        let probe = Probe::new().unwrap();
        let path = probe.path.join("outside");
        fs::write(&path, b"outside").unwrap();
        let held = File::open(&path).unwrap();
        let device = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/dev")
            .unwrap();
        assert!(
            open_beneath(
                &device,
                &format!("fd/{}", held.as_raw_fd()),
                libc::O_RDONLY,
                0
            )
            .is_err()
        );
        let descriptors = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/dev/fd")
            .unwrap();
        assert!(metadata_at(&descriptors, &held.as_raw_fd().to_string()).is_err());
        assert!(
            open_beneath(
                &device,
                &format!("fd/{}", probe.directory.as_raw_fd()),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0
            )
            .is_err()
        );
        assert!(metadata_at(&descriptors, &probe.directory.as_raw_fd().to_string()).is_err());
        drop(held);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn regular_files_cannot_be_used_as_intermediate_directories() {
        check_capabilities().unwrap();
        let mut probe = Probe::new().unwrap();
        let file = probe.path.join("file");
        fs::write(&file, b"original").unwrap();
        probe
            .entries
            .push(ProbeEntry::remember(&probe.directory, "file", false).unwrap());
        for path in ["file/child", "file/..namedfork/rsrc"] {
            assert!(open_beneath(&probe.directory, path, libc::O_RDONLY, 0).is_err());
        }
        assert_eq!(fs::read(file).unwrap(), b"original");
        probe.remove().unwrap();
    }
}
