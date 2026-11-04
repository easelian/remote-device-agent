//! The public CLI has one start action and two lifecycle commands.
use crate::error::{Error, Result};

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Help,
    Version,
    Launch,
    Unbind,
    Update,
}

pub fn parse(argv: &[String]) -> Result<Action> {
    match argv
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => Ok(Action::Launch),
        ["unbind"] => Ok(Action::Unbind),
        ["update"] => Ok(Action::Update),
        ["--help" | "-h"] => Ok(Action::Help),
        ["--version" | "-V"] => Ok(Action::Version),
        _ => Err(Error::message("Unknown command or option. Use rdh --help")),
    }
}

pub fn help() -> String {
    format!(
        "Remote Device Hub Agent {}\n\n  rdh          Start; open the browser and wait for approval when unbound\n  rdh unbind   Revoke access, disable automatic startup, and stop the Agent\n  rdh update   Update the Agent\n  rdh --help   Show this help\n  rdh --version  Show the version\n",
        env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn only_the_small_public_interface_is_accepted() {
        for (argv, expected) in [
            (vec![], Action::Launch),
            (vec!["unbind"], Action::Unbind),
            (vec!["update"], Action::Update),
            (vec!["--help"], Action::Help),
            (vec!["--version"], Action::Version),
        ] {
            assert_eq!(parse(&args(&argv)).unwrap(), expected);
        }
        for removed in [
            "--server",
            "--config",
            "--profile",
            "--root",
            "--name",
            "--no-browser",
            "--pair-only",
            "--recover-lock",
            "startup",
            "install-native",
            "install-service",
            "upgrade",
            "rollback",
            "validate-config",
            "service-run",
            "startup-run",
            "service-update-protocol",
            "bind",
            "rebind",
        ] {
            assert!(parse(&args(&[removed])).is_err(), "{removed}");
            assert!(parse(&args(&[removed, "--help"])).is_err(), "{removed}");
        }
        assert!(parse(&args(&["unbind", "--server", "https://other.example"])).is_err());
    }
}
