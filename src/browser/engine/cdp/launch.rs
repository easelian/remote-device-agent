use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[cfg(windows)]
pub(in crate::browser::engine) fn with_handles(
    args: &[OsString],
    input: u32,
    output: u32,
) -> Vec<OsString> {
    let mut command = args.to_vec();
    command.push(format!("--remote-debugging-io-pipes={input},{output}").into());
    command
}
pub(in crate::browser::engine) fn arguments(data: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--headless",
        "--remote-debugging-pipe",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--site-per-process",
        "--window-size=1280,720",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let mut directory = OsString::from("--user-data-dir=");
    directory.push(data);
    args.push(directory);
    args.push("about:blank".into());
    args
}

#[cfg(unix)]
pub fn detect(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_owned());
    }
    #[cfg(target_os = "linux")]
    let paths = [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/opt/google/chrome/chrome",
    ];
    #[cfg(target_os = "macos")]
    let paths = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    ];
    paths.iter().map(PathBuf::from).find(|p| p.is_file())
}

#[cfg(windows)]
pub fn detect(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = configured {
        return path.is_file().then(|| path.to_owned());
    }
    let mut candidates = Vec::new();
    for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(base) {
            for relative in [
                r"Google\Chrome\Application\chrome.exe",
                r"Microsoft\Edge\Application\msedge.exe",
                r"Chromium\Application\chrome.exe",
            ] {
                candidates.push(PathBuf::from(&base).join(relative));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub(in crate::browser::engine) fn specification(data: &Path) -> super::super::Launch {
    super::super::Launch {
        arguments: arguments(data),
        #[cfg(unix)]
        child_descriptors: (3, 4),
    }
}
