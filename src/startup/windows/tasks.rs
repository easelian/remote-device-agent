//! Task Scheduler COM interfaces stay on their initializing worker thread.
use std::{marker::PhantomData, rc::Rc, sync::OnceLock};
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoInitializeSecurity, CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                RPC_C_IMP_LEVEL_IMPERSONATE,
            },
            TaskScheduler::{
                IExecAction, ILogonTrigger, ITaskFolder, ITaskService, TASK_ACTION_EXEC,
                TASK_CREATE, TASK_INSTANCES_IGNORE_NEW, TASK_LOGON_INTERACTIVE_TOKEN,
                TASK_RUNLEVEL_LUA, TASK_TRIGGER_LOGON, TaskScheduler,
            },
            Variant::VARIANT,
        },
    },
    core::{BSTR, HRESULT, Interface, Result},
};

struct Apartment(PhantomData<Rc<()>>);
impl Apartment {
    fn new() -> Result<Self> {
        // Each successful initialization, including S_FALSE, is balanced on
        // this thread. An existing incompatible apartment is rejected.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        }
        let apartment = Self(PhantomData);
        static SECURITY: OnceLock<std::result::Result<(), HRESULT>> = OnceLock::new();
        let result = SECURITY.get_or_init(|| unsafe {
            CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            )
            .map_err(|error| error.code())
        });
        result
            .as_ref()
            .map_err(|code| windows::core::Error::from_hresult(*code))?;
        Ok(apartment)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct Scheduler {
    folder: ITaskFolder,
    service: ITaskService,
    // Fields drop in declaration order, so every COM reference above is
    // released before uninitializing the apartment.
    _apartment: Apartment,
}
impl Scheduler {
    fn connect() -> Result<Self> {
        let apartment = Apartment::new()?;
        // These interfaces remain confined to this synchronous call/thread.
        let service: ITaskService =
            unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)? };
        let empty = VARIANT::default();
        unsafe {
            service.Connect(&empty, &empty, &empty, &empty)?;
        }
        let folder = unsafe { service.GetFolder(&BSTR::from("\\"))? };
        Ok(Self {
            folder,
            service,
            _apartment: apartment,
        })
    }
}

pub(super) fn enabled(name: &str) -> Result<bool> {
    let scheduler = Scheduler::connect()?;
    let task = unsafe { scheduler.folder.GetTask(&BSTR::from(name))? };
    Ok(unsafe { task.Enabled()? }.0 != 0)
}
pub(super) fn set(name: &str, enabled: bool) -> Result<bool> {
    let scheduler = Scheduler::connect()?;
    let task = unsafe { scheduler.folder.GetTask(&BSTR::from(name))? };
    // Changes future startup only. No Run/Stop call is sent.
    unsafe {
        task.SetEnabled(VARIANT_BOOL(if enabled { -1 } else { 0 }))?;
    }
    Ok(unsafe { task.Enabled()? }.0 != 0)
}

pub(super) fn create(
    name: &str,
    sid: &str,
    executable: &str,
    arguments: &str,
    directory: &str,
) -> Result<bool> {
    let scheduler = Scheduler::connect()?;
    let empty = VARIANT::default();
    // All interfaces and borrowed BSTR/VARIANT arguments remain alive for the
    // synchronous API invocation. TASK_CREATE atomically refuses collisions.
    unsafe {
        let definition = scheduler.service.NewTask(0)?;
        let principal = definition.Principal()?;
        principal.SetUserId(&BSTR::from(sid))?;
        principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)?;
        principal.SetRunLevel(TASK_RUNLEVEL_LUA)?;
        let trigger: ILogonTrigger = definition.Triggers()?.Create(TASK_TRIGGER_LOGON)?.cast()?;
        trigger.SetUserId(&BSTR::from(sid))?;
        let action: IExecAction = definition.Actions()?.Create(TASK_ACTION_EXEC)?.cast()?;
        action.SetPath(&BSTR::from(executable))?;
        action.SetArguments(&BSTR::from(arguments))?;
        action.SetWorkingDirectory(&BSTR::from(directory))?;
        let settings = definition.Settings()?;
        settings.SetEnabled(VARIANT_BOOL(0))?;
        settings.SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)?;
        settings.SetExecutionTimeLimit(&BSTR::from("PT0S"))?;
        settings.SetDisallowStartIfOnBatteries(VARIANT_BOOL(0))?;
        settings.SetStopIfGoingOnBatteries(VARIANT_BOOL(0))?;
        let task = scheduler.folder.RegisterTaskDefinition(
            &BSTR::from(name),
            &definition,
            TASK_CREATE.0,
            &VARIANT::from(sid),
            &empty,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &empty,
        )?;
        Ok(task.Enabled()?.0 != 0)
    }
}

#[cfg(test)]
pub(super) fn remove(name: &str) -> Result<()> {
    let scheduler = Scheduler::connect()?;
    unsafe { scheduler.folder.DeleteTask(&BSTR::from(name), 0) }
}
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(super) struct Definition {
    pub executable: String,
    pub arguments: String,
    pub directory: String,
    pub last_run: f64,
    pub interactive: bool,
    pub limited: bool,
    pub trigger_user: String,
}
#[cfg(test)]
pub(super) fn inspect(name: &str) -> Result<Definition> {
    let scheduler = Scheduler::connect()?;
    unsafe {
        let task = scheduler.folder.GetTask(&BSTR::from(name))?;
        let definition = task.Definition()?;
        let action: IExecAction = definition.Actions()?.get_Item(1)?.cast()?;
        let trigger: ILogonTrigger = definition.Triggers()?.get_Item(1)?.cast()?;
        let principal = definition.Principal()?;
        let mut executable = BSTR::new();
        let mut arguments = BSTR::new();
        let mut directory = BSTR::new();
        let mut trigger_user = BSTR::new();
        let mut logon = Default::default();
        let mut run_level = Default::default();
        action.Path(&mut executable)?;
        action.Arguments(&mut arguments)?;
        action.WorkingDirectory(&mut directory)?;
        trigger.UserId(&mut trigger_user)?;
        principal.LogonType(&mut logon)?;
        principal.RunLevel(&mut run_level)?;
        Ok(Definition {
            executable: executable.to_string(),
            arguments: arguments.to_string(),
            directory: directory.to_string(),
            last_run: task.LastRunTime()?,
            interactive: logon == TASK_LOGON_INTERACTIVE_TOKEN,
            limited: run_level == TASK_RUNLEVEL_LUA,
            trigger_user: trigger_user.to_string(),
        })
    }
}
