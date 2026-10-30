//! Bounded synchronous pipes for the Windows process supervisor.
//! Parent ends never block; child ends retain ordinary blocking stdio semantics.
use std::ffi::c_void;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ,
    GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, IsValidSid, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING,
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_NOWAIT,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
};

pub(in crate::processes::native) struct Pipe(OwnedHandle);

struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: only successful LocalAlloc-based API outputs enter this owner.
        unsafe { LocalFree(self.0) };
    }
}

fn private_descriptor() -> io::Result<Local> {
    let mut token = null_mut();
    // SAFETY: pseudo-process handle and writable result remain valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful OpenProcessToken transfers one real handle.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut bytes = 0;
    // SAFETY: a sizing query writes bytes and no data buffer.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut bytes) };
    if bytes < size_of::<TOKEN_USER>() as u32 || bytes > 64 * 1024 {
        return Err(io::Error::other("Invalid process token information size"));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and has at least bytes writable storage.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful query initialized TOKEN_USER and its borrowed SID.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() || unsafe { IsValidSid(user.User.Sid) } == 0 {
        return Err(io::Error::other("Invalid process user SID"));
    }
    let mut text = null_mut();
    // SAFETY: token buffer stays alive through SID conversion.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 || text.is_null() {
        return Err(io::Error::last_os_error());
    }
    let text_owner = Local(text.cast());
    let mut length = 0;
    // SAFETY: the API returns a NUL-terminated SID, shorter than this bound.
    while length < 256 && unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    if length == 256 {
        return Err(io::Error::other("Invalid process SID text"));
    }
    // SAFETY: the bounded scan established the live string length.
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) })
        .map_err(io::Error::other)?;
    drop(text_owner);
    let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = null_mut();
    // SAFETY: a terminated SDDL string and writable API result remain live.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
        || descriptor.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(descriptor))
}

/// Only the returned child end is inheritable. The caller must include it in
/// CreateProcess's explicit HANDLE_LIST and drop its copy after child creation.
/// The server end uses synchronous NOWAIT I/O, with no pending OVERLAPPED buffer.
pub(super) fn pair(child_reads: bool) -> io::Result<(Pipe, OwnedHandle)> {
    let descriptor = private_descriptor()?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let name: Vec<u16> = format!(
        r"\\.\pipe\rdh-{}-{}",
        std::process::id(),
        crate::random::hex_token::<16>().map_err(|error| io::Error::other(error.to_string()))?,
    )
    .encode_utf16()
    .chain(Some(0))
    .collect();
    // SAFETY: descriptor/name stay live for creation. One private local instance
    // prevents connecting to an existing server with an attacker-chosen name.
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            (if child_reads {
                PIPE_ACCESS_OUTBOUND
            } else {
                PIPE_ACCESS_INBOUND
            }) | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            64 * 1024,
            64 * 1024,
            0,
            &attributes,
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: creation returned an owned, non-inheritable server handle.
    let server = unsafe { OwnedHandle::from_raw_handle(server) };
    attributes.bInheritHandle = 1;
    // SAFETY: the private server and its name remain live. The client uses
    // synchronous blocking I/O, as expected by an ordinary child program.
    let client = unsafe {
        CreateFileW(
            name.as_ptr(),
            if child_reads {
                GENERIC_READ
            } else {
                GENERIC_WRITE
            },
            0,
            &attributes,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if client == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: creation returned one owned inheritable client handle.
    let client = unsafe { OwnedHandle::from_raw_handle(client) };
    // SAFETY: NOWAIT never leaves a pending connect operation or buffer.
    if unsafe { ConnectNamedPipe(server.as_raw_handle(), null_mut()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            return Err(error);
        }
    }
    let mut peer = 0;
    // SAFETY: the connected pipe and writable PID are live. A random name alone
    // is not authentication: a racing same-user client must also be rejected.
    if unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut peer) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if peer != unsafe { GetCurrentProcessId() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unexpected stdio pipe client",
        ));
    }
    Ok((Pipe(server), client))
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut read = 0;
        // SAFETY: synchronous NOWAIT I/O borrows the buffer only for this call.
        if unsafe {
            ReadFile(
                self.0.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len().min(u32::MAX as usize) as u32,
                &mut read,
                null_mut(),
            )
        } != 0
        {
            // A zero-byte write at the other end can complete a pipe read
            // with zero bytes. Only the broken/disconnected status is EOF.
            return if read == 0 {
                Err(io::ErrorKind::WouldBlock.into())
            } else {
                Ok(read as usize)
            };
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_NO_DATA) => Err(io::ErrorKind::WouldBlock.into()),
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) => Ok(0),
            _ => Err(error),
        }
    }
}
impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut written = 0;
        // SAFETY: synchronous byte-mode NOWAIT I/O may accept a prefix or zero
        // bytes; it never retains this buffer after returning.
        if unsafe {
            WriteFile(
                self.0.as_raw_handle(),
                buffer.as_ptr(),
                buffer.len().min(u32::MAX as usize) as u32,
                &mut written,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            Err(io::ErrorKind::WouldBlock.into())
        } else {
            Ok(written as usize)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::{Duration, Instant};

    #[test]
    fn empty_output_is_nonblocking_and_eof_follows_the_last_byte() {
        let (mut parent, child) = pair(false).unwrap();
        let mut child = File::from(child);
        let mut buffer = [0; 16];
        assert_eq!(
            parent.read(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let mut written = 1;
        assert_ne!(
            unsafe {
                WriteFile(
                    child.as_raw_handle(),
                    b"".as_ptr(),
                    0,
                    &mut written,
                    null_mut(),
                )
            },
            0
        );
        assert_eq!(written, 0);
        assert_eq!(
            parent.read(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        child.write_all("😀bytes".as_bytes()).unwrap();
        drop(child);
        let mut result = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match parent.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => result.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline);
                    std::thread::yield_now();
                }
                other => panic!("Unexpected pipe read: {other:?}"),
            }
        }
        assert_eq!(result, "😀bytes".as_bytes());
    }

    #[test]
    fn blocked_input_reports_partial_writes_without_waiting_for_the_child() {
        let (mut parent, child) = pair(true).unwrap();
        let mut child = File::from(child);
        let bytes = [0x5a; 16384];
        let mut written = 0;
        let started = Instant::now();
        loop {
            assert!(
                written < 1024 * 1024,
                "Pipe buffer did not impose its bound"
            );
            match parent.write(&bytes) {
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                other => panic!("Unexpected pipe write: {other:?}"),
            }
        }
        assert!(written > 0);
        assert!(started.elapsed() < Duration::from_secs(3));
        let mut received = vec![0; written];
        child.read_exact(&mut received).unwrap();
        assert!(received.iter().all(|byte| *byte == 0x5a));
        assert!(parent.write(b"tail").unwrap() > 0);
        drop(child);
        assert!(parent.write(b"closed").is_err());
    }
}
