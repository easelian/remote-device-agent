//! Test-owned Task Scheduler registration; no system-default name is used.
use ::windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            TaskScheduler::{
                IExecAction, ILogonTrigger, IRegisteredTask, ITaskFolder, ITaskService,
                TASK_ACTION_EXEC, TASK_CREATE, TASK_INSTANCES_IGNORE_NEW,
                TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_LUA, TASK_TRIGGER_LOGON, TaskScheduler,
            },
            Variant::VARIANT,
        },
    },
    core::{BSTR, Interface},
};
use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

struct Apartment(PhantomData<Rc<()>>);
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
pub struct Task {
    registered: IRegisteredTask,
    folder: ITaskFolder,
    _service: ITaskService,
    _apartment: Apartment,
    pub name: String,
    started: bool,
    removed: bool,
}
impl Task {
    pub fn create(sid: &str, executable: &str, arguments: &str, directory: &str) -> Self {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let apartment = Apartment(PhantomData);
            let service: ITaskService =
                CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).unwrap();
            let empty = VARIANT::default();
            service.Connect(&empty, &empty, &empty, &empty).unwrap();
            let folder = service.GetFolder(&BSTR::from("\\")).unwrap();
            let definition = service.NewTask(0).unwrap();
            let principal = definition.Principal().unwrap();
            principal.SetUserId(&BSTR::from(sid)).unwrap();
            principal
                .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
                .unwrap();
            principal.SetRunLevel(TASK_RUNLEVEL_LUA).unwrap();
            let trigger: ILogonTrigger = definition
                .Triggers()
                .unwrap()
                .Create(TASK_TRIGGER_LOGON)
                .unwrap()
                .cast()
                .unwrap();
            trigger.SetUserId(&BSTR::from(sid)).unwrap();
            let action: IExecAction = definition
                .Actions()
                .unwrap()
                .Create(TASK_ACTION_EXEC)
                .unwrap()
                .cast()
                .unwrap();
            action.SetPath(&BSTR::from(executable)).unwrap();
            action.SetArguments(&BSTR::from(arguments)).unwrap();
            action.SetWorkingDirectory(&BSTR::from(directory)).unwrap();
            let settings = definition.Settings().unwrap();
            settings.SetEnabled(VARIANT_BOOL(0)).unwrap();
            settings.SetExecutionTimeLimit(&BSTR::from("PT0S")).unwrap();
            settings
                .SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)
                .unwrap();
            settings
                .SetDisallowStartIfOnBatteries(VARIANT_BOOL(0))
                .unwrap();
            settings.SetStopIfGoingOnBatteries(VARIANT_BOOL(0)).unwrap();
            let name = format!(
                "RDH-Test-Update-{}",
                rdh_agent::random::hex_token::<10>().unwrap()
            );
            let registered = folder
                .RegisterTaskDefinition(
                    &BSTR::from(&name),
                    &definition,
                    TASK_CREATE.0,
                    &VARIANT::from(sid),
                    &empty,
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &empty,
                )
                .unwrap();
            Self {
                registered,
                folder,
                _service: service,
                _apartment: apartment,
                name,
                started: false,
                removed: false,
            }
        }
    }
    pub fn inspect(&self) -> (String, String, String, bool, f64) {
        unsafe {
            let task = self.folder.GetTask(&BSTR::from(&self.name)).unwrap();
            let definition = task.Definition().unwrap();
            let action: IExecAction = definition
                .Actions()
                .unwrap()
                .get_Item(1)
                .unwrap()
                .cast()
                .unwrap();
            let (mut path, mut arguments, mut directory) = (BSTR::new(), BSTR::new(), BSTR::new());
            action.Path(&mut path).unwrap();
            action.Arguments(&mut arguments).unwrap();
            action.WorkingDirectory(&mut directory).unwrap();
            (
                path.to_string(),
                arguments.to_string(),
                directory.to_string(),
                task.Enabled().unwrap().0 != 0,
                task.LastRunTime().unwrap(),
            )
        }
    }
    pub fn set_enabled(&self, enabled: bool) {
        unsafe {
            self.registered
                .SetEnabled(VARIANT_BOOL(if enabled { -1 } else { 0 }))
                .unwrap();
        }
    }
    pub fn start(&mut self) {
        // Run can have an uncertain result; cleanup is required from here.
        self.started = true;
        unsafe {
            self.folder
                .GetTask(&BSTR::from(&self.name))
                .unwrap()
                .Run(&VARIANT::default())
                .unwrap();
        }
    }
    pub fn execution_state(&self) -> (bool, i32) {
        use ::windows::Win32::System::TaskScheduler::{TASK_STATE_QUEUED, TASK_STATE_RUNNING};
        unsafe {
            let task = self.folder.GetTask(&BSTR::from(&self.name)).unwrap();
            (
                matches!(
                    task.State().unwrap(),
                    TASK_STATE_QUEUED | TASK_STATE_RUNNING
                ) || task.GetInstances(0).unwrap().Count().unwrap() != 0,
                task.LastTaskResult().unwrap(),
            )
        }
    }
    fn stop_owned(&mut self) -> ::windows::core::Result<()> {
        unsafe {
            let task = self.folder.GetTask(&BSTR::from(&self.name))?;
            task.Stop(0)?;
            let deadline = Instant::now() + Duration::from_secs(15);
            while task.GetInstances(0)?.Count()? != 0 {
                if Instant::now() >= deadline {
                    return Err(::windows::core::Error::from_hresult(
                        ::windows::core::HRESULT(0x800705b4u32 as i32),
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.started = false;
        Ok(())
    }
    pub fn stop(&mut self) {
        self.stop_owned().unwrap();
    }
    pub fn security(&self) -> String {
        unsafe {
            self.registered
                .GetSecurityDescriptor(7)
                .unwrap()
                .to_string()
        }
    }
    pub fn fingerprint(&self) -> String {
        unsafe {
            let task = self.folder.GetTask(&BSTR::from(&self.name)).unwrap();
            let definition = task.Definition().unwrap();
            let action: IExecAction = definition
                .Actions()
                .unwrap()
                .get_Item(1)
                .unwrap()
                .cast()
                .unwrap();
            action
                .SetPath(&BSTR::from("RDH-NATIVE-VERSION-PLACEHOLDER"))
                .unwrap();
            let mut xml = BSTR::new();
            definition.XmlText(&mut xml).unwrap();
            rdh_agent::config::sha256_hex(
                &serde_json::to_vec(&(xml.to_string(), self.security(), self.inspect().3)).unwrap(),
            )
        }
    }
    pub fn migration_fingerprint(&self) -> String {
        unsafe {
            let task = self.folder.GetTask(&BSTR::from(&self.name)).unwrap();
            let definition = task.Definition().unwrap();
            let action: IExecAction = definition
                .Actions()
                .unwrap()
                .get_Item(1)
                .unwrap()
                .cast()
                .unwrap();
            action
                .SetPath(&BSTR::from("RDH-NATIVE-VERSION-PLACEHOLDER"))
                .unwrap();
            action
                .SetArguments(&BSTR::from("RDH-MIGRATION-ARGUMENTS-PLACEHOLDER"))
                .unwrap();
            let mut xml = BSTR::new();
            definition.XmlText(&mut xml).unwrap();
            rdh_agent::config::sha256_hex(
                &serde_json::to_vec(&(xml.to_string(), self.security(), self.inspect().3)).unwrap(),
            )
        }
    }
    pub fn replace_action(&self, sid: &str, executable: &str, arguments: &str) {
        use ::windows::Win32::System::TaskScheduler::{
            TASK_DONT_ADD_PRINCIPAL_ACE, TASK_IGNORE_REGISTRATION_TRIGGERS, TASK_UPDATE,
        };
        unsafe {
            let task = self.folder.GetTask(&BSTR::from(&self.name)).unwrap();
            assert_eq!(task.GetInstances(0).unwrap().Count().unwrap(), 0);
            let definition = task.Definition().unwrap();
            let action: IExecAction = definition
                .Actions()
                .unwrap()
                .get_Item(1)
                .unwrap()
                .cast()
                .unwrap();
            action.SetPath(&BSTR::from(executable)).unwrap();
            action.SetArguments(&BSTR::from(arguments)).unwrap();
            self.folder
                .RegisterTaskDefinition(
                    &BSTR::from(&self.name),
                    &definition,
                    TASK_UPDATE.0
                        | TASK_DONT_ADD_PRINCIPAL_ACE.0
                        | TASK_IGNORE_REGISTRATION_TRIGGERS.0,
                    &VARIANT::from(sid),
                    &VARIANT::default(),
                    TASK_LOGON_INTERACTIVE_TOKEN,
                    &VARIANT::default(),
                )
                .unwrap();
        }
    }
    pub fn remove(mut self) {
        assert!(!self.started, "Stop the fixture Task before removing it");
        unsafe {
            self.folder.DeleteTask(&BSTR::from(&self.name), 0).unwrap();
        }
        self.removed = true;
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        if !self.removed {
            if self.started
                && let Err(error) = self.stop_owned()
            {
                eprintln!("Retaining running Task fixture {}: {error}", self.name);
                return;
            }
            if let Err(error) = unsafe { self.folder.DeleteTask(&BSTR::from(&self.name), 0) } {
                eprintln!("Task fixture cleanup failed for {}: {error}", self.name);
            }
        }
    }
}
