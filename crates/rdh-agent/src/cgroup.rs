//! Conservative cgroup-v2 isolation check before stopping a managed Agent.
//! A process that detached from its process group may still be killed when a
//! systemd service exits, so a finished Agent job is not sufficient evidence.

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CString, OsString};
    use std::fs::{self, File};
    use std::io::Read;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path, PathBuf};

    use crate::error::{Error, Result};

    const MAX_DIRS: usize = 512;
    const MAX_ENTRIES: usize = 32_768;
    const MAX_PROCS_FILE: u64 = 256 * 1024;

    fn read_limited(path: &Path, limit: u64) -> Result<String> {
        let mut data = String::new();
        File::open(path)?
            .take(limit + 1)
            .read_to_string(&mut data)?;
        if data.len() as u64 > limit {
            return Err(Error::message("Cgroup metadata is too large"));
        }
        Ok(data)
    }

    fn absolute_clean(path: &Path) -> bool {
        path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
    }
    fn membership(contents: &str) -> Result<PathBuf> {
        if contents.lines().count() != 1 {
            return Err(Error::message(
                "Only a unified cgroup-v2 hierarchy is supported",
            ));
        }
        let mut entries = contents.lines().filter_map(|line| line.strip_prefix("0::"));
        let value = entries
            .next()
            .ok_or_else(|| Error::message("A unified cgroup is required"))?;
        let path = PathBuf::from(value);
        if entries.next().is_some() || !absolute_clean(&path) {
            return Err(Error::message("Invalid unified cgroup membership"));
        }
        Ok(path)
    }
    fn unescape_mount(value: &str) -> Result<PathBuf> {
        let bytes = value.as_bytes();
        let mut result = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] == b'\\' {
                if at + 3 >= bytes.len()
                    || !bytes[at + 1..at + 4]
                        .iter()
                        .all(|b| (b'0'..=b'7').contains(b))
                {
                    return Err(Error::message("Invalid cgroup mount path"));
                }
                let n = (bytes[at + 1] - b'0') as u16 * 64
                    + (bytes[at + 2] - b'0') as u16 * 8
                    + (bytes[at + 3] - b'0') as u16;
                if n == 0 || n > 255 {
                    return Err(Error::message("Invalid cgroup mount path"));
                }
                result.push(n as u8);
                at += 4;
            } else {
                result.push(bytes[at]);
                at += 1;
            }
        }
        let path = PathBuf::from(OsString::from_vec(result));
        if !absolute_clean(&path) {
            return Err(Error::message("Invalid cgroup mount path"));
        }
        Ok(path)
    }
    fn unit_boundary(group: &Path) -> Result<PathBuf> {
        let parts: Vec<_> = group
            .components()
            .filter_map(|part| match part {
                Component::Normal(value) => Some(value),
                _ => None,
            })
            .collect();
        let mut prefix = PathBuf::from("/");
        for (index, part) in parts.iter().enumerate() {
            prefix.push(part);
            let name = part
                .to_str()
                .ok_or_else(|| Error::message("Invalid systemd cgroup name"))?;
            let user_manager = index >= 2
                && parts[index - 2] == "user.slice"
                && name
                    .strip_prefix("user@")
                    .and_then(|v| v.strip_suffix(".service"))
                    .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
                    .is_some_and(|id| {
                        parts[index - 1]
                            .to_str()
                            .is_some_and(|parent| parent == format!("user-{id}.slice"))
                    });
            if !user_manager && (name.ends_with(".service") || name.ends_with(".scope")) {
                return Ok(prefix);
            }
        }
        Err(Error::message(
            "A systemd service or scope cgroup is required",
        ))
    }
    fn group_path(group: &Path, mountinfo: &str) -> Result<PathBuf> {
        for line in mountinfo.lines() {
            let Some((before, after)) = line.split_once(" - ") else {
                continue;
            };
            if after.split_whitespace().next() != Some("cgroup2") {
                continue;
            }
            let fields: Vec<_> = before.split_whitespace().collect();
            if fields.len() < 5 {
                continue;
            }
            let root = unescape_mount(fields[3])?;
            let mount = unescape_mount(fields[4])?;
            let Ok(relative) = group.strip_prefix(&root) else {
                continue;
            };
            let mount = mount.canonicalize()?;
            let target = mount.join(relative).canonicalize()?;
            if !target.starts_with(&mount) {
                return Err(Error::message("Invalid cgroup location"));
            }
            let path = CString::new(target.as_os_str().as_bytes())
                .map_err(|_| Error::message("Invalid cgroup location"))?;
            let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: path is NUL-terminated and stat is writable for statfs.
            if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            if i128::from(stat.f_type) != i128::from(libc::CGROUP2_SUPER_MAGIC) {
                return Err(Error::message("The cgroup mount is not cgroup v2"));
            }
            return Ok(target);
        }
        Err(Error::message("The Agent cgroup is not visible"))
    }
    fn scan_group(root: &Path, pid: u32) -> Result<bool> {
        let mut stack = vec![root.to_path_buf()];
        let mut dirs = 0;
        let mut entries = 0;
        let mut found_self = false;
        while let Some(directory) = stack.pop() {
            dirs += 1;
            if dirs > MAX_DIRS {
                return Err(Error::message(
                    "Cgroup inspection exceeds its directory limit",
                ));
            }
            let mut data = String::new();
            File::open(directory.join("cgroup.procs"))?
                .take(MAX_PROCS_FILE + 1)
                .read_to_string(&mut data)?;
            if data.len() as u64 > MAX_PROCS_FILE {
                return Err(Error::message("Cgroup process list is too large"));
            }
            for line in data.lines() {
                let value = line
                    .parse::<u32>()
                    .map_err(|_| Error::message("Invalid cgroup process list"))?;
                if value != pid {
                    return Ok(false);
                }
                found_self = true;
            }
            for entry in fs::read_dir(&directory)? {
                entries += 1;
                if entries > MAX_ENTRIES {
                    return Err(Error::message("Cgroup inspection exceeds its entry limit"));
                }
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    return Err(Error::message("Unexpected link in cgroup"));
                }
                if kind.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        Ok(found_self)
    }

    pub fn only_current_process() -> Result<bool> {
        let before = membership(&read_limited(Path::new("/proc/self/cgroup"), 16 * 1024)?)?;
        // Scan the containing unit, including sibling subgroups. Looking only
        // at the Agent's current subgroup could miss detached service members.
        let target = group_path(
            &unit_boundary(&before)?,
            &read_limited(Path::new("/proc/self/mountinfo"), 4 * 1024 * 1024)?,
        )?;
        let isolated = scan_group(&target, std::process::id())?;
        let after = membership(&read_limited(Path::new("/proc/self/cgroup"), 16 * 1024)?)?;
        if before != after {
            return Err(Error::message("Agent cgroup changed during inspection"));
        }
        Ok(isolated)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn scans_subgroups_and_refuses_unknown_or_other_processes() {
            let root = std::env::temp_dir().join(format!(
                "rdh-cgroup-{}",
                crate::random::hex_token::<8>().unwrap()
            ));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("cgroup.procs"), b"123\n").unwrap();
            fs::create_dir(root.join("child")).unwrap();
            fs::write(root.join("child/cgroup.procs"), b"").unwrap();
            assert!(scan_group(&root, 123).unwrap());
            fs::write(root.join("child/cgroup.procs"), b"456\n").unwrap();
            assert!(!scan_group(&root, 123).unwrap());
            fs::write(root.join("child/cgroup.procs"), b"0\n").unwrap();
            assert!(!scan_group(&root, 123).unwrap());
            fs::remove_file(root.join("child/cgroup.procs")).unwrap();
            assert!(scan_group(&root, 123).is_err());
            fs::remove_dir_all(root).unwrap();
        }
        #[test]
        fn rejects_ambiguous_membership_and_decodes_mount_paths() {
            assert_eq!(
                membership("0::/user.slice/a.service\n").unwrap(),
                Path::new("/user.slice/a.service")
            );
            assert!(membership("0::/a/../b\n").is_err());
            assert!(membership("0::/a\n0::/b\n").is_err());
            assert!(membership("0::/a\n1:name=systemd:/other\n").is_err());
            assert_eq!(
                unescape_mount("/run/a\\040b").unwrap(),
                Path::new("/run/a b")
            );
            assert!(unescape_mount("/run/bad\\999").is_err());
            assert_eq!(
                unit_boundary(Path::new("/system.slice/rdh.service/work/fake.service")).unwrap(),
                Path::new("/system.slice/rdh.service")
            );
            assert_eq!(
                unit_boundary(Path::new(
                    "/user.slice/user-1000.slice/user@1000.service/app.slice/rdh.service/child"
                ))
                .unwrap(),
                Path::new("/user.slice/user-1000.slice/user@1000.service/app.slice/rdh.service")
            );
            assert_eq!(
                unit_boundary(Path::new("/user.slice/user-1000.slice/session-3.scope")).unwrap(),
                Path::new("/user.slice/user-1000.slice/session-3.scope")
            );
            assert!(unit_boundary(Path::new("/unmanaged/group")).is_err());
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::only_current_process;
