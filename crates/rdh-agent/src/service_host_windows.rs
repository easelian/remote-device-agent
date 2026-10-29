use crate::{
    cli::{Action, LaunchArgs, parse},
    error::{Error, Result},
    startup::valid_name,
};
use std::{
    ffi::c_void,
    io,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicPtr, Ordering},
    },
};
use tokio::sync::watch;
use windows_sys::Win32::{
    Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR},
    System::Services::{
        RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
        SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
        SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STOP_PENDING,
        SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
        StartServiceCtrlDispatcherW,
    },
};

struct Context {
    name: Vec<u16>,
    args: LaunchArgs,
    state_directory: Option<PathBuf>,
    stop: watch::Sender<bool>,
    shutdown: watch::Receiver<bool>,
    // SCM owns this opaque handle for the process lifetime. The status mutex
    // serializes updates; the pointer is never dereferenced by Rust.
    handle: AtomicPtr<c_void>,
    status: Mutex<SERVICE_STATUS>,
}
static CONTEXT: OnceLock<Context> = OnceLock::new();

impl Context {
    fn report(&self, status: &SERVICE_STATUS) -> io::Result<()> {
        // SAFETY: RegisterServiceCtrlHandlerExW published this live handle;
        // the borrowed status stays valid for this synchronous call.
        if unsafe { SetServiceStatus(self.handle.load(Ordering::Acquire), status) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn transition(&self, next: u32) -> io::Result<()> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if status.dwCurrentState == SERVICE_STOPPED
            || (status.dwCurrentState == SERVICE_STOP_PENDING && next == SERVICE_RUNNING)
        {
            return Ok(());
        }
        if status.dwCurrentState == next && next == SERVICE_STOP_PENDING {
            return Ok(());
        }
        status.dwCurrentState = next;
        status.dwControlsAccepted = if next == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        };
        let pending = matches!(next, SERVICE_START_PENDING | SERVICE_STOP_PENDING);
        status.dwCheckPoint = if pending { 1 } else { 0 };
        // No timer fabricates checkpoint progress while cleanup is blocked.
        status.dwWaitHint = if pending { 30_000 } else { 0 };
        self.report(&status)
    }
}

fn arguments(
    args: &[String],
    server: Option<&str>,
) -> Result<(String, LaunchArgs, Option<PathBuf>)> {
    let (name, remaining) = match args {
        [flag, name, remaining @ ..] if flag == "--service-name" && valid_name(name) => {
            (name, remaining)
        }
        _ => {
            return Err(Error::message(
                "Use service-run --service-name NAME --config ABSOLUTE_PATH --server URL",
            ));
        }
    };
    let mut launch_args = Vec::new();
    let mut state_directory = None;
    let mut items = remaining.iter();
    while let Some(argument) = items.next() {
        if argument == "--state-directory" {
            let directory = items.next().filter(|value| Path::new(value).is_absolute() && !value.chars().any(char::is_control))
                .ok_or_else(|| Error::message("--state-directory requires an absolute Unicode path without control characters"))?;
            if state_directory.replace(PathBuf::from(directory)).is_some() {
                return Err(Error::message("Duplicate service state directory"));
            }
        } else {
            launch_args.push(argument.clone());
        }
    }
    let Action::Launch(mut launch) = parse(&launch_args, server)? else {
        return Err(Error::message("A service requires Agent launch arguments"));
    };
    if launch.pair_only
        || launch.name.is_some()
        || !launch.roots.is_empty()
        || launch
            .config
            .as_deref()
            .is_none_or(|path| !Path::new(path).is_absolute())
    {
        return Err(Error::message(
            "A service requires an absolute existing --config and cannot change pairing",
        ));
    }
    launch.no_browser = true;
    Ok((name.clone(), launch, state_directory))
}

pub(super) fn run(args: &[String], server: Option<&str>) -> Result<()> {
    let (name, args, state_directory) = arguments(args, server)?;
    let (stop, shutdown) = watch::channel(false);
    CONTEXT
        .set(Context {
            name: name.encode_utf16().chain(Some(0)).collect(),
            args,
            state_directory,
            stop,
            shutdown,
            handle: AtomicPtr::new(null_mut()),
            status: Mutex::new(SERVICE_STATUS {
                dwServiceType: SERVICE_WIN32_OWN_PROCESS,
                ..SERVICE_STATUS::default()
            }),
        })
        .map_err(|_| Error::message("Service dispatcher can only run once"))?;
    let context = CONTEXT.get().expect("service context initialized");
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: context.name.as_ptr().cast_mut(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // Connect promptly. Configuration, runtime and network setup all happen
    // in ServiceMain, never on the SCM dispatcher thread.
    if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn fatal(error: impl std::fmt::Display) -> ! {
    eprintln!("Remote Device Hub service: {error}");
    use windows_sys::Win32::System::EventLog::{
        DeregisterEventSource, EVENTLOG_ERROR_TYPE, RegisterEventSourceW, ReportEventW,
    };
    let source: Vec<u16> = "RemoteDeviceHub".encode_utf16().chain(Some(0)).collect();
    let message: Vec<u16> = error
        .to_string()
        .chars()
        .take(2048)
        .collect::<String>()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // The error is a bounded local diagnostic, never the launch arguments,
    // device configuration or remote command body. No registry source is
    // installed here; Windows retains the supplied string in Application log.
    unsafe {
        let event = RegisterEventSourceW(null(), source.as_ptr());
        if !event.is_null() {
            let strings = [message.as_ptr()];
            ReportEventW(
                event,
                EVENTLOG_ERROR_TYPE,
                0,
                1,
                null_mut(),
                1,
                0,
                strings.as_ptr(),
                null(),
            );
            DeregisterEventSource(event);
        }
    }
    // Failed or unconfirmed cleanup must not be reported as SERVICE_STOPPED.
    // Process exit makes SCM observe failure, closes all owned Job handles,
    // and leaves any retained profile lock available for explicit recovery.
    std::process::exit(1)
}

unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
    if std::panic::catch_unwind(service_main_inner).is_err() {
        fatal("Service panicked; inspect the profile before recovery");
    }
}
fn service_main_inner() {
    let context = CONTEXT.get().expect("service context initialized");
    // The context is static; SCM never receives a pointer to a stack object.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(context.name.as_ptr(), Some(control), null()) };
    if handle.is_null() {
        fatal(io::Error::last_os_error());
    }
    context.handle.store(handle, Ordering::Release);
    context
        .transition(SERVICE_START_PENDING)
        .unwrap_or_else(|error| fatal(error));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map_or(1, |count| count.get().min(4)))
        .enable_all()
        .build()
        .unwrap_or_else(|error| fatal(error));
    let config = context
        .args
        .config
        .as_ref()
        .expect("validated service config");
    crate::app::validate_config(&["--config".into(), config.clone()])
        .unwrap_or_else(|error| fatal(error));
    // RUNNING means this service can handle controls. Hub readiness is
    // established by the authenticated session, independently of SCM state.
    context
        .transition(SERVICE_RUNNING)
        .unwrap_or_else(|error| fatal(error));
    runtime
        .block_on(crate::app::launch_existing(
            context.args.clone(),
            context.shutdown.clone(),
            context.stop.clone(),
            context.state_directory.clone(),
        ))
        .unwrap_or_else(|error| fatal(error));
    drop(runtime);
    // App shutdown has drained jobs and released the profile locks. This is
    // the last operation: Windows may terminate us once STOPPED is reported.
    context
        .transition(SERVICE_STOPPED)
        .unwrap_or_else(|error| fatal(error));
}

unsafe extern "system" fn control(code: u32, _: u32, _: *mut c_void, _: *mut c_void) -> u32 {
    match std::panic::catch_unwind(|| control_inner(code)) {
        Ok(result) => result,
        Err(_) => fatal("Service control handler panicked"),
    }
}
fn control_inner(code: u32) -> u32 {
    let context = CONTEXT.get().expect("service context initialized");
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            // The handler only signals the owning runtime. It never waits for
            // network, a child process, or cleanup on the dispatcher thread.
            context.stop.send_replace(true);
            match context.transition(SERVICE_STOP_PENDING) {
                Ok(()) => NO_ERROR,
                Err(error) => error.raw_os_error().unwrap_or(1) as u32,
            }
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unattended_service_requires_existing_explicit_identity() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };
        let base = args(&[
            "--service-name",
            "RDH-fixture",
            "--config",
            r"C:\private data\device.json",
            "--server",
            "http://localhost:8111",
        ]);
        let (name, launch, state_directory) = arguments(&base, None).unwrap();
        assert_eq!(name, "RDH-fixture");
        assert!(launch.no_browser);
        assert!(state_directory.is_none());
        let mut with_state = base.clone();
        with_state.extend(args(&["--state-directory", r"C:\private data\state"]));
        assert_eq!(
            arguments(&with_state, None).unwrap().2,
            Some(PathBuf::from(r"C:\private data\state"))
        );
        with_state.extend(args(&["--state-directory", r"C:\second"]));
        assert!(arguments(&with_state, None).is_err());
        for extra in [
            vec!["--pair-only"],
            vec!["--name", "new identity"],
            vec!["--root", r"work=C:\data"],
            vec!["--state-directory", "relative"],
        ] {
            let mut invalid = base.clone();
            invalid.extend(args(&extra));
            assert!(arguments(&invalid, None).is_err());
        }
        assert!(arguments(&args(&["--service-name", "RDH-fixture"]), None).is_err());
        assert!(
            arguments(
                &args(&["--service-name", "RDH-fixture", "--config", "relative.json"]),
                None
            )
            .is_err()
        );
    }
}
