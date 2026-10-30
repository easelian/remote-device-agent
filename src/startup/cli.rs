//! Local management commands for future startup and remote approvals.
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde_json::json;

use super::grant;
use super::native::{DEFAULT_SERVICE_NAME, NativeStartup};
use crate::cli::{self, Action};
use crate::config::{DeviceConfig, ProfilePaths, config_base};
use crate::error::{Error, Result};
use crate::startup::{Binding, Mode, Startup, StartupBackend, StartupFailure, valid_name};

fn failure(error: StartupFailure) -> Error {
    Error::message(format!("{} ({})", error.message, error.code))
}
fn confirm(message: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::message(
            "Local confirmation requires an interactive terminal",
        ));
    }
    print!("{message}\nType yes to confirm: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        return Err(Error::message("Startup change cancelled"));
    }
    Ok(())
}
fn help() {
    print!(
        "rdh startup status [--config PATH | --server URL --profile NAME]\n\
        rdh startup set none|user|service [--config PATH]\n\
        rdh startup approve REQUEST_ID [--config PATH]\n\
        rdh startup deny REQUEST_ID [--config PATH]\n\
        rdh startup grant user|service [--config PATH]  (administrator)\n\
        rdh startup revoke [--config PATH]             (administrator)\n\
        rdh startup bind user|service NAME [--config PATH]\n\
        rdh startup system status|set none|service [--name NAME]\n\n\
        Enabling requires interactive local confirmation or an administrator grant for remote requests.\n\
        Changes affect future startup only; the current Agent is not stopped or started.\n"
    );
}

fn system(args: &[String]) -> Result<()> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    let mut index = usize::from(!args.is_empty());
    let mode = if action == "set" {
        let raw = args.get(index).ok_or_else(|| {
            Error::message("Use startup system status|set none|service [--name NAME]")
        })?;
        index += 1;
        Some(Mode::parse(raw).map_err(failure)?)
    } else if action == "status" {
        None
    } else {
        return Err(Error::message(
            "Use startup system status|set none|service [--name NAME]",
        ));
    };
    let mut name = DEFAULT_SERVICE_NAME;
    if args.get(index).is_some_and(|value| value == "--name") && args.len() == index + 2 {
        name = &args[index + 1];
        index += 2;
    }
    if index != args.len() || !valid_name(name) || mode == Some(Mode::User) {
        return Err(Error::message(
            "Use startup system status|set none|service [--name NAME]",
        ));
    }
    let backend = NativeStartup::new(std::env::current_dir()?.canonicalize()?)?;
    let binding = Binding {
        mode: Mode::Service,
        name: name.into(),
    };
    if let Some(mode) = mode {
        if !backend.service_authority() {
            return Err(Error::message(
                "Run this command as an administrator; automatic elevation is disabled",
            ));
        }
        if mode == Mode::Service {
            confirm(&format!(
                "Enable the existing boot service {name}? Its current process will remain running."
            ))?;
        }
        backend.set(&binding, mode == Mode::Service)?;
    }
    println!(
        "{}",
        json!({"mode":if backend.enabled(&binding)? {"service"} else {"none"},
        "registration":binding,"current_connection":"unchanged"})
    );
    Ok(())
}

pub fn run(args: &[String], server: Option<&str>) -> Result<()> {
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        help();
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "system") {
        return system(&args[1..]);
    }
    let command = args.first().map(String::as_str).unwrap_or("status");
    if ![
        "status", "set", "approve", "deny", "grant", "revoke", "bind",
    ]
    .contains(&command)
    {
        return Err(Error::message("Unknown startup command"));
    }
    let mut index = usize::from(!args.is_empty());
    let value = if ["set", "approve", "deny", "grant", "bind"].contains(&command) {
        let value = args
            .get(index)
            .ok_or_else(|| Error::message("A startup command argument is required"))?;
        index += 1;
        Some(value.as_str())
    } else {
        None
    };
    let name = if command == "bind" {
        let name = args.get(index).ok_or_else(|| {
            Error::message("bind requires user|service and a native registration name")
        })?;
        index += 1;
        Some(name.as_str())
    } else {
        None
    };
    let Action::Launch(options) = cli::parse(&args[index..], server)? else {
        return Err(Error::message("Invalid startup profile options"));
    };
    let base = if options.config.is_some() {
        None
    } else {
        Some(config_base()?)
    };
    let paths = ProfilePaths::resolve(
        base.as_deref().unwrap_or_else(|| Path::new("")),
        &options.server,
        &options.profile,
        options.config.as_deref(),
    )?;
    let config = paths.destination.canonicalize()?;
    if command == "grant" || command == "revoke" {
        let mode = if command == "revoke" {
            Mode::None
        } else {
            Mode::parse(value.unwrap()).map_err(failure)?
        };
        if command == "grant" && mode == Mode::None {
            return Err(Error::message("grant requires user or service"));
        }
        grant::set_grant(&config, mode)?;
        println!("{}", json!({"status":"applied","managed_mode":mode}));
        return Ok(());
    }
    DeviceConfig::read(&config)?;
    let startup = Startup::new(&config)?;
    let closing = AtomicBool::new(false);
    let deadline = || Instant::now() + Duration::from_secs(30);
    let result = match command {
        "status" => startup.status(),
        "bind" => startup.bind(
            Mode::parse(value.unwrap()).map_err(failure)?,
            name.unwrap(),
            deadline(),
            &closing,
        ),
        "set" => {
            let mode = Mode::parse(value.unwrap()).map_err(failure)?;
            if mode != Mode::None {
                confirm(&format!(
                    "Enable {mode:?} startup for {}? The current connection will remain running.",
                    config.display()
                ))?;
            }
            startup.set(mode, true, deadline(), &closing)
        }
        "approve" | "deny" => {
            let id = value.unwrap();
            if command == "approve" {
                let status = startup.status().map_err(failure)?;
                let pending = &status["pending"];
                if pending["request_id"].as_str() != Some(id) {
                    return Err(Error::message("No matching pending startup request"));
                }
                confirm(&format!(
                    "Approve remote request {id}: enable {} startup for {}?",
                    pending["mode"].as_str().unwrap_or("unknown"),
                    config.display()
                ))?;
            }
            startup.approve(id, command == "approve", deadline(), &closing)
        }
        _ => unreachable!(),
    }
    .map_err(failure)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
