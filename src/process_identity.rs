//! Process birth identity for local ownership; inspecting it never sends a signal.
use crate::error::{Error, Result};

pub(crate) fn current() -> Result<String> {
    started(std::process::id())?
        .ok_or_else(|| Error::message("Cannot establish Agent process identity"))
}

pub(crate) fn running(pid: u32, identity: &str) -> Result<bool> {
    if identity.is_empty() || identity.len() > 128 {
        return Err(Error::message("Invalid Agent process identity"));
    }
    Ok(started(pid)?.as_deref() == Some(identity))
}

fn started(pid: u32) -> Result<Option<String>> {
    if pid == 0 {
        return Err(Error::message("Invalid Agent process identity"));
    }
    platform_started(pid)
}

#[cfg(target_os = "linux")]
fn platform_started(pid: u32) -> Result<Option<String>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // The command may contain spaces and closing parentheses. Fields after its
    // final delimiter begin at state (3); starttime is field 22.
    let fields: Vec<_> = stat
        .rsplit_once(") ")
        .ok_or_else(|| Error::message("Invalid process status"))?
        .1
        .split_whitespace()
        .collect();
    if matches!(fields.first(), Some(&"Z" | &"X")) {
        return Ok(None);
    }
    let ticks = fields
        .get(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| Error::message("Invalid process start time"))?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok(Some(format!("{}:{ticks}", boot.trim())))
}

#[cfg(target_os = "macos")]
fn platform_started(pid: u32) -> Result<Option<String>> {
    if pid > i32::MAX as u32 {
        return Err(Error::message("Invalid Agent process identity"));
    }
    // SAFETY: proc_bsdinfo is plain data, and proc_pidinfo receives its exact
    // writable size. The result is read only after a complete structure arrived.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let received = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if received != size {
        let error = std::io::Error::last_os_error();
        return if received == 0 && error.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(Error::message(format!(
                "Cannot inspect process start time: {error}"
            )))
        };
    }
    // Darwin sys/proc.h defines SZOMB as 5; libc does not expose this constant.
    const SZOMB: u32 = 5;
    if info.pbi_status == SZOMB {
        return Ok(None);
    }
    Ok(Some(format!(
        "{}:{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )))
}

#[cfg(windows)]
fn platform_started(pid: u32) -> Result<Option<String>> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_INVALID_PARAMETER, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            WaitForSingleObject,
        },
    };
    // SAFETY: a successful OpenProcess returns a uniquely owned handle. No
    // termination permission is requested, and no process is signalled.
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            Ok(None)
        } else {
            Err(error.into())
        };
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => return Ok(None),
        WAIT_TIMEOUT => {}
        _ => return Err(std::io::Error::last_os_error().into()),
    }
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all four outputs are initialized FILETIME values and handle lives
    // until after the synchronous query.
    if unsafe {
        GetProcessTimes(
            handle.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(Some(format!(
        "{:08x}{:08x}",
        created.dwHighDateTime, created.dwLowDateTime
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_started(_: u32) -> Result<Option<String>> {
    Err(Error::message(
        "Process identity is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_pid_requires_the_same_birth_identity() {
        let identity = current().unwrap();
        assert_eq!(current().unwrap(), identity);
        assert!(running(std::process::id(), &identity).unwrap());
        assert!(!running(std::process::id(), &(identity + "-previous")).unwrap());
        assert!(running(0, "invalid").is_err());
        assert!(running(std::process::id(), "").is_err());
    }

    #[test]
    fn exited_child_cannot_keep_ownership() {
        const CHILD: &str = "RDH_PROCESS_IDENTITY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(30));
            return;
        }
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!(
                        "{}::exited_child_cannot_keep_ownership",
                        module_path!().split_once("::").unwrap().1
                    ),
                ])
                .env(CHILD, "1")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let identity = started(pid).unwrap().expect("live child");
        assert!(running(pid, &identity).unwrap());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!running(pid, &identity).unwrap());
    }
}
