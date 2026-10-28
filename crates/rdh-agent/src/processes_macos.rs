//! Darwin process-group observations. The caller keeps its child unreaped.
//!
//! A libproc failure, a full buffer, or an absent leader cannot establish
//! completed cleanup. This observes the original group; escaped processes are
//! outside that boundary, as they are with the Linux process-group backend.
use std::{collections::BTreeSet, io};

struct Member {
    pid: u32,
    group: u32,
    zombie: bool,
}

fn check_snapshot(
    leader: u32,
    buffer: &[libc::pid_t],
    bytes: i32,
    mut lookup: impl FnMut(u32) -> io::Result<Member>,
) -> io::Result<bool> {
    let bytes = usize::try_from(bytes)
        .map_err(|_| io::Error::other("Invalid process-group snapshot length"))?;
    let width = std::mem::size_of::<libc::pid_t>();
    if bytes == 0 || bytes % width != 0 || bytes / width >= buffer.len() {
        return Err(io::Error::other("Incomplete process-group snapshot"));
    }
    let members = &buffer[..bytes / width];
    let mut unique = BTreeSet::new();
    for &pid in members {
        if pid <= 0 || !unique.insert(pid as u32) {
            return Err(io::Error::other("Invalid process-group member identity"));
        }
    }
    if leader == 0 || !unique.contains(&leader) {
        return Err(io::Error::other("Unreaped process-group leader is absent"));
    }
    // Require a readable, matching leader even when all other members vanished.
    let held = lookup(leader)?;
    if held.pid != leader || held.group != leader {
        return Err(io::Error::other("Process-group leader identity changed"));
    }
    if !held.zombie {
        return Ok(true);
    }
    // The kernel enumerates group members under its process-list lock. Require
    // the pinned zombie to be the ONLY member, rather than querying descendants
    // one by one and racing their final forks. Lingering zombies keep cleanup
    // pending as well; there is no weaker fallback if they cannot be reaped.
    Ok(unique.len() != 1)
}

#[cfg(all(target_os = "macos", feature = "experimental-macos-processes"))]
fn process_info(pid: u32) -> io::Result<Member> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // Nonzero arg requests zombie lookup as well as ordinary processes.
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/proc_info.c
    let bytes = unsafe {
        *libc::__error() = 0;
        libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if bytes != std::mem::size_of::<libc::proc_bsdinfo>() as i32 {
        let error = io::Error::last_os_error();
        return Err(if bytes == 0 && error.raw_os_error() != Some(0) {
            error
        } else {
            io::Error::other("Incomplete process member information")
        });
    }
    Ok(Member {
        pid: info.pbi_pid,
        group: info.pbi_pgid,
        zombie: info.pbi_status == libc::SZOMB,
    })
}

#[cfg(all(target_os = "macos", feature = "experimental-macos-processes"))]
pub(super) fn group_has_live_members(leader: u32) -> io::Result<bool> {
    const PROC_PGRP_ONLY: u32 = 2; // Public libproc.h / sys/proc_info.h filter.
    const MAX_MEMBERS: usize = 65_536;
    if leader == 0 || leader > libc::pid_t::MAX as u32 {
        return Err(io::Error::other("Invalid process-group leader"));
    }
    let mut buffer = vec![0 as libc::pid_t; MAX_MEMBERS];
    let bytes = unsafe {
        *libc::__error() = 0;
        libc::proc_listpids(
            PROC_PGRP_ONLY,
            leader,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(buffer.as_slice()) as libc::c_int,
        )
    };
    // libproc returns zero on syscall error, as well as on an empty list.
    // A pinned leader makes an empty group invalid for this cleanup check.
    if bytes == 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() != Some(0) {
            error
        } else {
            io::Error::other("Unreaped process-group leader is absent")
        });
    }
    check_snapshot(leader, &buffer, bytes, process_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zombie(pid: u32) -> Member {
        Member {
            pid,
            group: 10,
            zombie: true,
        }
    }

    #[test]
    fn incomplete_or_inconsistent_snapshots_never_confirm_cleanup() {
        for (buffer, bytes) in [
            (vec![10, 0], -1),
            (vec![10, 0], 0),
            (vec![10, 0], 1),
            (vec![10, 0], 8),
            (vec![10, 0], 12),
            (vec![11, 0], 4),
            (vec![10, 10, 0], 8),
            (vec![10, -1, 0], 8),
            (vec![10, 0, 0], 8),
        ] {
            assert!(check_snapshot(10, &buffer, bytes, |pid| Ok(zombie(pid))).is_err());
        }
        assert!(check_snapshot(0, &[10, 0], 4, |pid| Ok(zombie(pid))).is_err());
    }

    #[test]
    fn uncertain_leader_lookups_keep_cleanup_pending() {
        for errno in [libc::ESRCH, libc::EACCES, libc::EPERM, libc::EIO] {
            assert!(
                check_snapshot(10, &[10, 0], 4, |_| {
                    Err(io::Error::from_raw_os_error(errno))
                })
                .is_err()
            );
        }
        assert!(check_snapshot(10, &[10, 0], 4, |_| Ok(zombie(11))).is_err());
        assert!(
            check_snapshot(10, &[10, 0], 4, |pid| {
                Ok(Member {
                    group: 12,
                    ..zombie(pid)
                })
            })
            .is_err()
        );
    }

    #[test]
    fn only_the_pinned_zombie_alone_can_confirm_cleanup() {
        assert!(!check_snapshot(10, &[10, 0], 4, |pid| Ok(zombie(pid))).unwrap());
        // Even a zombie descendant must actually disappear from the group.
        assert!(
            check_snapshot(10, &[10, 11, 0], 8, |pid| {
                assert_eq!(pid, 10); // No per-descendant query can permit cleanup.
                Ok(zombie(pid))
            })
            .unwrap()
        );
        assert!(
            check_snapshot(10, &[10, 0], 4, |pid| {
                Ok(Member {
                    zombie: false,
                    ..zombie(pid)
                })
            })
            .unwrap()
        );
    }

    #[cfg(all(target_os = "macos", feature = "experimental-macos-processes"))]
    #[test]
    fn native_member_information_matches_current_process() {
        let pid = std::process::id();
        let member = process_info(pid).unwrap();
        assert_eq!(member.pid, pid);
        assert_eq!(member.group, unsafe { libc::getpgrp() } as u32);
        assert!(!member.zombie);
    }
}
