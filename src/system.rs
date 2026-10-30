pub struct SystemInfo {
    pub os: &'static str,
    pub hostname: String,
    pub machine: String,
    pub release: String,
}

/// Best-effort local browser launch with background child reaping.
pub(crate) fn open_browser(uri: &str) {
    if std::env::var_os("SSH_CONNECTION").is_some() {
        return;
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    #[cfg(target_os = "linux")]
    let command = ("xdg-open".to_owned(), vec![uri.to_owned()]);
    #[cfg(target_os = "macos")]
    let command = ("open".to_owned(), vec![uri.to_owned()]);
    #[cfg(windows)]
    let command = {
        let executable = std::env::var_os("SystemRoot")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
            .join("System32")
            .join("rundll32.exe");
        (
            executable.to_string_lossy().into_owned(),
            vec!["url.dll,FileProtocolHandler".to_owned(), uri.to_owned()],
        )
    };
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    if let Ok(mut child) = tokio::process::Command::new(command.0)
        .args(command.1)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

#[cfg(unix)]
pub fn info() -> SystemInfo {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    let mut name = MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: uname initializes the supplied utsname on success.
    let result = unsafe { libc::uname(name.as_mut_ptr()) };
    if result == 0 {
        // SAFETY: uname succeeded, and its fields are NUL-terminated C arrays.
        let name = unsafe { name.assume_init() };
        let field = |value: &[libc::c_char]| {
            // SAFETY: each utsname field is NUL-terminated by the OS.
            unsafe { CStr::from_ptr(value.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        let hostname = field(&name.nodename);
        return SystemInfo {
            os: if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            },
            hostname: if hostname.is_empty() {
                "device".into()
            } else {
                hostname
            },
            machine: field(&name.machine),
            release: field(&name.release),
        };
    }
    SystemInfo {
        os: if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        },
        hostname: "device".into(),
        machine: std::env::consts::ARCH.into(),
        release: String::new(),
    }
}

#[cfg(windows)]
pub fn info() -> SystemInfo {
    SystemInfo {
        os: "Windows",
        hostname: windows::hostname().unwrap_or_else(|| "device".into()),
        machine: std::env::consts::ARCH.into(),
        release: windows::release().unwrap_or_default(),
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
    use windows_sys::Win32::Networking::WinSock::{GetHostNameW, WSACleanup, WSADATA, WSAStartup};
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

    struct Winsock;

    impl Drop for Winsock {
        fn drop(&mut self) {
            // SAFETY: balance only this query's successful WSAStartup. Other socket
            // owners retain their own references, including Rust's networking.
            unsafe { WSACleanup() };
        }
    }

    pub(super) fn hostname() -> Option<String> {
        let mut data = WSADATA::default();
        // SAFETY: data is a writable WSADATA and 0x0202 requests Winsock 2.2.
        if unsafe { WSAStartup(0x0202, &mut data) } != 0 {
            return None;
        }
        let _winsock = Winsock;
        if data.wVersion != 0x0202 {
            return None;
        }
        // GetHostNameW documents 256 UTF-16 code units as sufficient for all
        // namespace providers. Like the legacy Node API, this honors provider
        // and cluster names instead of trusting the COMPUTERNAME variable.
        let mut buffer = [0u16; 256];
        // SAFETY: the buffer has the supplied length and Winsock is initialized.
        if unsafe { GetHostNameW(buffer.as_mut_ptr(), buffer.len() as i32) } != 0 {
            return None;
        }
        let end = buffer.iter().position(|unit| *unit == 0)?;
        if end == 0 {
            return None;
        }
        String::from_utf16(&buffer[..end]).ok()
    }

    pub(super) fn release() -> Option<String> {
        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
            ..OSVERSIONINFOW::default()
        };
        // SAFETY: version has the requested structure size and writable storage.
        // The legacy Node implementation also uses ntdll's RtlGetVersion rather
        // than the manifest-sensitive GetVersionEx compatibility view.
        if unsafe { RtlGetVersion(&mut version) } != 0 {
            return None;
        }
        Some(format!(
            "{}.{}.{}",
            version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
        ))
    }

    #[cfg(test)]
    mod tests {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::Duration;

        #[test]
        fn system_queries_preserve_existing_socket_owners() {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            client
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            for _ in 0..4 {
                assert!(!super::hostname().unwrap().is_empty());
                let release = super::release().unwrap();
                let parts: Vec<_> = release.split('.').collect();
                assert_eq!(parts.len(), 3, "{release}");
                assert!(parts.iter().all(|part| part.parse::<u32>().is_ok()));
                assert_ne!(parts[0], "0");
            }
            client.write_all(b"alive").unwrap();
            let mut data = [0u8; 5];
            peer.read_exact(&mut data).unwrap();
            assert_eq!(&data, b"alive");
        }
    }
}
