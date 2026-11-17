//! Linux process-group observation; errors never mean an empty group.
#[cfg(any(test, feature = "experimental-browser-bidi"))]
use std::fs;
use std::io;
pub fn prepare() -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(any(test, feature = "experimental-browser-bidi"))]
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
#[cfg(any(test, feature = "experimental-browser-bidi"))]
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
#[cfg(any(test, feature = "experimental-browser-bidi"))]
pub fn stopped(group: u32) -> io::Result<bool> {
    let list = members(group)?;
    if !list.iter().any(|(pid, _)| *pid == group) {
        return Err(io::Error::other("Unreaped browser leader is absent"));
    }
    if !list.iter().all(|(_, state)| matches!(state, 'Z' | 'X')) {
        return Ok(false);
    }
    // A descendant may exit after the caller's first reap pass. Keep the
    // leader pinned until adopted descendants have also been collected; a
    // zombie cannot use the profile, but it still belongs to this supervisor.
    reap(group);
    Ok(members(group)?.iter().all(|(pid, _)| *pid == group))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::CommandExt,
        process::Command,
        time::{Duration, Instant},
    };

    #[test]
    fn stopped_reaps_adopted_descendants_before_releasing_the_leader() {
        prepare().unwrap();
        let mut leader = Command::new("/bin/sh")
            .args(["-c", "sleep 0.02 &"])
            .process_group(0)
            .spawn()
            .unwrap();
        let group = leader.id();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let list = members(group).unwrap();
            if list.len() >= 2 && list.iter().all(|(_, state)| matches!(state, 'Z' | 'X')) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not reach unreaped descendant state: {list:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let result = stopped(group).unwrap();
        let remaining = members(group).unwrap();
        reap(group);
        leader.wait().unwrap();
        assert!(result);
        assert_eq!(
            remaining.len(),
            1,
            "successful stop must leave only the pinned leader: {remaining:?}"
        );
        assert_eq!(remaining[0].0, group);
        assert!(members(group).unwrap().is_empty());
    }
}
