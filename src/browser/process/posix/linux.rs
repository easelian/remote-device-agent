//! Linux process-group observation; errors never mean an empty group.
use std::{fs, io};
pub fn prepare() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn members(group: u32) -> io::Result<Vec<(u32, char)>> {
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => continue,
            Err(e) => return Err(e),
        };
        let fields: Vec<_> = stat
            .rsplit_once(") ")
            .ok_or_else(|| io::Error::other("Invalid process status"))?
            .1
            .split_whitespace()
            .collect();
        let pgrp = fields
            .get(2)
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| io::Error::other("Invalid process group"))?;
        if pgrp == group {
            members.push((
                pid,
                fields
                    .first()
                    .and_then(|s| s.chars().next())
                    .ok_or_else(|| io::Error::other("Invalid process state"))?,
            ));
        }
    }
    Ok(members)
}
pub fn reap(group: u32) {
    if let Ok(members) = members(group) {
        for (pid, _) in members {
            if pid != group {
                unsafe {
                    libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
        }
    }
}
pub fn stopped(group: u32) -> io::Result<bool> {
    let list = members(group)?;
    if !list.iter().any(|(pid, _)| *pid == group) {
        return Err(io::Error::other("Unreaped browser leader is absent"));
    }
    Ok(list.iter().all(|(_, state)| matches!(state, 'Z' | 'X')))
}
pub fn retired(group: u32) -> io::Result<bool> {
    Ok(members(group)?
        .iter()
        .all(|(_, state)| matches!(state, 'Z' | 'X')))
}
