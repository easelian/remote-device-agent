pub struct SystemInfo {
    pub os: &'static str,
    pub hostname: String,
    pub machine: String,
    pub release: String,
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

#[cfg(not(unix))]
pub fn info() -> SystemInfo {
    SystemInfo {
        os: "Windows",
        hostname: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "device".into()),
        machine: std::env::consts::ARCH.into(),
        release: String::new(),
    }
}
