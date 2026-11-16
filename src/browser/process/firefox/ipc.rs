//! Inherited, unnamed SOCK_SEQPACKET channel. No filesystem or TCP rendezvous.
use serde_json::Value;
use std::{
    io, mem,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

const MAX_PACKET: usize = 8192;
const MAX_FDS: usize = 2;

pub(super) fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

pub(super) fn send(fd: RawFd, value: &Value, descriptors: &[RawFd]) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_PACKET || descriptors.len() > MAX_FDS {
        return Err(io::Error::other("bidi_ipc_limit"));
    }
    let mut vector = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0usize; 8];
    let mut header: libc::msghdr = unsafe { mem::zeroed() };
    header.msg_iov = &mut vector;
    header.msg_iovlen = 1;
    if !descriptors.is_empty() {
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen =
            unsafe { libc::CMSG_SPACE(mem::size_of_val(descriptors) as _) as _ };
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of_val(descriptors) as _) as _;
            std::ptr::copy_nonoverlapping(
                descriptors.as_ptr(),
                libc::CMSG_DATA(cmsg).cast(),
                descriptors.len(),
            );
        }
    }
    let count = unsafe { libc::sendmsg(fd, &header, libc::MSG_NOSIGNAL) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize != bytes.len() {
        return Err(io::Error::other("bidi_ipc_short_write"));
    }
    Ok(())
}

// Linux libc uses size_t here on GNU and socklen_t on musl. The bounded
// conversions must compile with both layouts, including their equal-width case.
#[allow(clippy::unnecessary_cast)]
pub(super) fn receive(fd: RawFd) -> io::Result<(Value, Vec<OwnedFd>)> {
    let mut bytes = [0u8; MAX_PACKET];
    let mut control = [0usize; 8];
    let mut vector = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut header: libc::msghdr = unsafe { mem::zeroed() };
    header.msg_iov = &mut vector;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = mem::size_of_val(&control) as _;
    let count =
        unsafe { libc::recvmsg(fd, &mut header, libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    // Own every delivered descriptor before checking data/truncation, so every
    // malformed packet closes its handles as well as rejecting its contents.
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&header);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let size = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                for index in 0..size / mem::size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(
                        *libc::CMSG_DATA(cmsg).cast::<RawFd>().add(index),
                    ));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&header, cmsg);
        }
    }
    if count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "bidi_owner_gone",
        ));
    }
    if header.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || fds.len() > MAX_FDS {
        return Err(io::Error::other("bidi_ipc_limit"));
    }
    Ok((serde_json::from_slice(&bytes[..count as usize])?, fds))
}

pub(super) async fn read(
    fd: &tokio::io::unix::AsyncFd<OwnedFd>,
) -> io::Result<(Value, Vec<OwnedFd>)> {
    loop {
        let mut ready = fd.readable().await?;
        match ready.try_io(|inner| receive(inner.get_ref().as_raw_fd())) {
            Ok(result) => return result,
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn transferred_descriptors_are_private_and_packets_are_bounded() {
        let (left, right) = pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        send(
            left.as_raw_fd(),
            &json!({"kind":"fixture"}),
            &[file.as_raw_fd()],
        )
        .unwrap();
        let (value, descriptors) = receive(right.as_raw_fd()).unwrap();
        assert_eq!(value, json!({"kind":"fixture"}));
        assert_eq!(descriptors.len(), 1);
        assert_ne!(
            unsafe { libc::fcntl(descriptors[0].as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert!(send(left.as_raw_fd(), &json!("x".repeat(MAX_PACKET)), &[]).is_err());
        let oversized = vec![b'x'; MAX_PACKET + 1];
        assert_eq!(
            unsafe {
                libc::send(
                    left.as_raw_fd(),
                    oversized.as_ptr().cast(),
                    oversized.len(),
                    libc::MSG_NOSIGNAL,
                )
            },
            oversized.len() as isize
        );
        assert!(receive(right.as_raw_fd()).is_err());
    }
}
