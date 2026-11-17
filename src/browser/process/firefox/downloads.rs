//! A private tmpfs in the browser's mount namespace, read through inherited IPC.
use std::{
    ffi::CString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
};

pub(crate) struct Directory {
    pub path: PathBuf,
    directory: File,
}
impl Directory {
    pub fn receive(path: PathBuf, fd: OwnedFd) -> io::Result<Self> {
        let directory = File::from(fd);
        if !directory.metadata()?.is_dir() {
            return Err(io::Error::other("browser_download_storage"));
        }
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(directory.as_raw_fd(), &mut stat) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if stat.f_type as i128 != libc::TMPFS_MAGIC as i128
            || (stat.f_blocks as u64).saturating_mul(stat.f_bsize as u64)
                > crate::browser::files::MAX_FILE
        {
            return Err(io::Error::other("browser_download_storage"));
        }
        Ok(Self { path, directory })
    }
    pub fn open(&self, path: &str) -> io::Result<File> {
        let relative = Path::new(path)
            .strip_prefix(&self.path)
            .map_err(io::Error::other)?;
        if relative.components().count() != 1
            || !matches!(
                relative.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            return Err(io::Error::other("browser_download_path"));
        }
        let name = CString::new(relative.as_os_str().as_bytes())?;
        let fd = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let stat = file.metadata()?;
        if !stat.is_file() || stat.len() > crate::browser::files::MAX_FILE {
            return Err(io::Error::other("browser_download_file"));
        }
        Ok(file)
    }
    pub fn clear(&self) -> io::Result<()> {
        // /proc/self/fd resolves our inherited directory, never the host mountpoint.
        for entry in
            std::fs::read_dir(format!("/proc/self/fd/{}", self.directory.as_raw_fd()))?.take(129)
        {
            let entry = entry?;
            let name = CString::new(entry.file_name().as_bytes())?;
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

pub(super) fn mount(path: &Path) -> io::Result<OwnedFd> {
    crate::storage::create_private_dir(path).map_err(io::Error::other)?;
    let target = CString::new(path.as_os_str().as_bytes())?;
    if unsafe {
        libc::mount(
            c"tmpfs".as_ptr(),
            target.as_ptr(),
            c"tmpfs".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            c"size=33554432,nr_inodes=128,mode=0700".as_ptr().cast(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe {
        libc::open(
            target.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
#[tokio::test]
#[ignore = "requires the exact native Firefox helper and an explicit evidence directory"]
async fn native_download_tmpfs_enforces_capacity_without_host_files() {
    use std::io::Write;
    let root = std::env::temp_dir().join(format!(
        "rdh-download-capacity-{}",
        crate::random::hex_token::<12>().unwrap()
    ));
    crate::storage::create_private_dir(&root).unwrap();
    let helper = PathBuf::from(std::env::var_os("RDH_BIDI_HELPER").unwrap());
    let firefox = PathBuf::from(std::env::var_os("RDH_FIREFOX_EXECUTABLE").unwrap());
    let mut owner =
        super::OwnedFirefox::spawn_options(&helper, &firefox, &root, None, None, true).unwrap();
    let ready = owner.ready().await.unwrap();
    let directory = owner.downloads.as_ref().unwrap().clone();
    let fd = unsafe {
        libc::openat(
            directory.directory.as_raw_fd(),
            c"capacity.bin".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        )
    };
    assert!(fd >= 0);
    let mut file = unsafe { File::from_raw_fd(fd) };
    for _ in 0..512 {
        file.write_all(&[0; 65536]).unwrap();
    }
    assert_eq!(
        file.write_all(&[1]).unwrap_err().raw_os_error(),
        Some(libc::ENOSPC)
    );
    assert_eq!(
        file.metadata().unwrap().len(),
        crate::browser::files::MAX_FILE
    );
    assert_eq!(std::fs::read_dir(&directory.path).unwrap().count(), 0);
    assert!(
        directory
            .open(&format!("{}/../capacity.bin", directory.path.display()))
            .is_err()
    );
    assert!(directory.open("/etc/passwd").is_err());
    drop(file);
    directory.clear().unwrap();
    let mut small_files = 0;
    for index in 0..129 {
        let name = CString::new(format!("inode-{index}")).unwrap();
        let fd = unsafe {
            libc::openat(
                directory.directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOSPC)
            );
            break;
        }
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        small_files += 1;
    }
    assert!((1..128).contains(&small_files));
    directory.clear().unwrap();
    drop(directory);
    drop(ready);
    owner.close(false).await.unwrap();
    drop(owner);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    std::fs::remove_dir(root).unwrap();
    let evidence =
        PathBuf::from(std::env::var_os("RDH_D3_EVIDENCE").unwrap()).join("download-capacity.json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(evidence)
        .unwrap();
    writeln!(file,"{}",serde_json::json!({"capacity_bytes":crate::browser::files::MAX_FILE,"overflow_errno":"ENOSPC","small_file_limit":small_files,"host_visible_files":0,"owned_run_directories":0})).unwrap();
}
