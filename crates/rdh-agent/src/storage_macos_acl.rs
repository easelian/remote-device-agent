//! Conservative extended-ACL check for program and migration storage.
use crate::error::{Error, Result};
use std::path::Path;

pub(crate) fn check(path: &Path) -> Result<()> {
    use std::{
        ffi::c_void,
        fs::OpenOptions,
        os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    };
    // Apple Libc include/sys/acl.h and posix1e/acl_entry.c: extended ACLs
    // supplement mode bits. Reject write-granting entries, including inherited
    // entries; harmless read/deny entries need no identity interpretation.
    unsafe extern "C" {
        fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_int) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut c_void,
            kind: libc::c_int,
            entry: *mut *mut c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> libc::c_int;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW_ANY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            Error::message(format!(
                "Cannot open protected path {}: {error}",
                path.display()
            ))
        })?;
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
    if acl.is_null() {
        let error = std::io::Error::last_os_error();
        // Apple Libc acl_file.c -> filesec_get_property(FILESEC_ACL) reports
        // ENOENT when the valid descriptor has no extended ACL. Opening first
        // keeps that case distinct from an absent or symlinked path.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(Error::message(format!(
                "Cannot inspect ACL for {}: {error}",
                path.display()
            )))
        };
    }
    let result = (|| {
        for i in 0..=128 {
            let mut entry = std::ptr::null_mut();
            if unsafe { acl_get_entry(acl, if i == 0 { 0 } else { -1 }, &mut entry) } != 0 {
                let error = std::io::Error::last_os_error();
                return if error.raw_os_error() == Some(libc::EINVAL) {
                    Ok(())
                } else {
                    Err(error.into())
                };
            }
            let mut tag = 0;
            let mut mask = 0;
            if i == 128
                || unsafe { acl_get_tag_type(entry, &mut tag) } != 0
                || unsafe { acl_get_permset_mask_np(entry, &mut mask) } != 0
            {
                return Err(Error::message("Cannot inspect protected path ACL"));
            }
            const WRITES: u64 = (1 << 2)
                | (1 << 4)
                | (1 << 5)
                | (1 << 6)
                | (1 << 8)
                | (1 << 10)
                | (1 << 12)
                | (1 << 13);
            if !matches!(tag, 1 | 2) || (tag == 1 && mask & WRITES != 0) {
                return Err(Error::message(
                    "Path ACL grants write access; choose a protected directory",
                ));
            }
        }
        unreachable!()
    })();
    unsafe {
        acl_free(acl);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        process::Command,
    };

    #[test]
    fn distinguishes_absent_acls_from_missing_paths_and_rejects_write_grants() {
        let directory = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rdh-acl-{}",
            crate::random::hex_token::<12>().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let file = directory.join("program");
        fs::write(&file, b"fixture").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let chmod = |args: &[&str], path: &Path| {
            let output = Command::new("/bin/chmod")
                .args(args)
                .arg(path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        chmod(&["-N"], &file);
        check(&file).unwrap();
        assert!(check(&directory.join("absent")).is_err());
        let link = directory.join("link");
        symlink(&file, &link).unwrap();
        assert!(check(&link).is_err());
        chmod(&["+a", "everyone deny delete"], &file);
        check(&file).unwrap();
        chmod(&["+a", "everyone allow write"], &file);
        assert!(
            check(&file)
                .unwrap_err()
                .to_string()
                .contains("ACL grants write")
        );
        chmod(&["-N"], &file);
        check(&file).unwrap();
        chmod(&["+a", "everyone allow add_file"], &directory);
        assert!(check(&directory).is_err());
        chmod(&["-N"], &directory);
        fs::remove_dir_all(directory).unwrap();
    }
}
