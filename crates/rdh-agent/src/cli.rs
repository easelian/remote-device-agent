use crate::config::{DEFAULT_SERVER, validate_gateway};
use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchArgs {
    pub server: String,
    pub profile: String,
    pub no_browser: bool,
    pub recover_lock: bool,
    pub pair_only: bool,
    pub roots: Vec<String>,
    pub config: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Help,
    Version,
    Startup(Vec<String>),
    InstallNative(Vec<String>),
    InstallService(Vec<String>),
    Migrate(Vec<String>),
    Upgrade(Vec<String>),
    Rollback(Vec<String>),
    ValidateConfig(Vec<String>),
    ServiceUpdateProtocol,
    ServiceRun(Vec<String>),
    Launch(LaunchArgs),
}

pub fn parse(argv: &[String], environment_server: Option<&str>) -> Result<Action> {
    match argv.first().map(String::as_str) {
        Some("startup") => return Ok(Action::Startup(argv[1..].to_vec())),
        Some("install-native") => return Ok(Action::InstallNative(argv[1..].to_vec())),
        Some("install-service") => return Ok(Action::InstallService(argv[1..].to_vec())),
        Some("migrate") => return Ok(Action::Migrate(argv[1..].to_vec())),
        Some("upgrade") => return Ok(Action::Upgrade(argv[1..].to_vec())),
        Some("rollback") => return Ok(Action::Rollback(argv[1..].to_vec())),
        Some("validate-config") => return Ok(Action::ValidateConfig(argv[1..].to_vec())),
        Some("service-run") => return Ok(Action::ServiceRun(argv[1..].to_vec())),
        Some("service-update-protocol") if argv.len() == 1 => {
            return Ok(Action::ServiceUpdateProtocol);
        }
        _ => {}
    }
    if argv.iter().any(|arg| arg == "--version") {
        return Ok(Action::Version);
    }
    if argv.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(Action::Help);
    }
    let mut result = LaunchArgs {
        server: environment_server.unwrap_or(DEFAULT_SERVER).to_owned(),
        profile: "default".to_owned(),
        no_browser: false,
        recover_lock: false,
        pair_only: false,
        roots: Vec::new(),
        config: None,
        name: None,
    };
    let mut args = argv.iter();
    while let Some(item) = args.next() {
        match item.as_str() {
            "--" => continue,
            "--no-browser" => {
                result.no_browser = true;
                continue;
            }
            "--recover-lock" => {
                result.recover_lock = true;
                continue;
            }
            "--pair-only" => {
                result.pair_only = true;
                continue;
            }
            _ => {}
        }
        let Some(raw) = item.strip_prefix("--") else {
            return Err(Error::message(format!("Unknown argument: {item}")));
        };
        let (key, inline) = raw
            .split_once('=')
            .map_or((raw, None), |(key, value)| (key, Some(value)));
        if !matches!(key, "server" | "root" | "name" | "profile" | "config") {
            return Err(Error::message(format!("Unknown argument: {item}")));
        }
        let value = inline.or_else(|| args.next().map(String::as_str));
        let value = value
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| Error::message(format!("Option --{key} requires a value")))?;
        match key {
            "server" => result.server = value.to_owned(),
            "root" => result.roots.push(value.to_owned()),
            "name" => result.name = Some(value.to_owned()),
            "profile" => result.profile = value.to_owned(),
            "config" => result.config = Some(value.to_owned()),
            _ => unreachable!(),
        }
    }
    result.server = validate_gateway(&result.server)?;
    Ok(Action::Launch(result))
}

pub fn help() -> String {
    format!(
        "Remote Device Hub Agent {} (Rust development build)\n\n\
        --server URL       Use a specific Hub (otherwise RDH_SERVER or {})\n\
        --root NAME=PATH   Expose a file root; repeat as needed\n\
        --name NAME        Device display name\n\
        --profile NAME     Separate device profile (default: default)\n\
        --config PATH      Custom private configuration file\n\
        --no-browser       Print the pairing URL without opening a browser\n\
        --recover-lock     Recover a stale lock after its process has exited\n\
        --pair-only        Exit after pairing without starting the Agent\n\n\
        Commands:\n  rdh startup --help         Manage future startup and local approvals\n\
          rdh install-native --help  Install a standalone development binary\n\
          rdh install-service --help Install a native Windows LocalService registration\n\
          rdh migrate --help         Inspect or switch a stopped legacy service\n\
          rdh upgrade --help         Check or switch native versions offline\n\
          rdh rollback --help        Restore the previous native version offline\n\
          rdh validate-config --config PATH  Check a configuration locally\n",
        env!("CARGO_PKG_VERSION"),
        DEFAULT_SERVER
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn parses_legacy_flags_and_preserves_multiple_roots() {
        let Action::Launch(args) = parse(
            &args(&[
                "--server=http://localhost:8111",
                "--root",
                "work=/tmp/a b",
                "--root=docs=/tmp/docs",
                "--profile",
                "example",
                "--no-browser",
                "--pair-only",
                "--recover-lock",
            ]),
            None,
        )
        .unwrap() else {
            panic!("expected launch")
        };
        assert_eq!(args.server, "http://localhost:8111");
        assert_eq!(args.profile, "example");
        assert_eq!(args.roots, ["work=/tmp/a b", "docs=/tmp/docs"]);
        assert!(args.no_browser && args.pair_only && args.recover_lock);
    }

    #[test]
    fn rejects_unknown_or_valueless_flags() {
        assert!(parse(&args(&["--server", "--no-browser"]), None).is_err());
        assert!(parse(&args(&["--unknown"]), None).is_err());
        assert!(parse(&[], Some("")).is_err());
    }

    #[test]
    fn version_precedes_other_flags_like_the_existing_cli() {
        assert_eq!(
            parse(&args(&["--unknown", "--help", "--version"]), None).unwrap(),
            Action::Version
        );
    }
}
