# Remote Device Hub Agent

English | [简体中文](README-zhCN.md)

Connect a Linux, macOS or Windows device to Remote Device Hub for remote file,
search and process operations under the device user's authority. The Agent is a
standalone native executable named `rdh`.

## Install and run

### Install with a script

**Linux and macOS** require `curl` and either `sha256sum` or `shasum`:

```sh
curl -fsSL https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.sh | sh
```

**Windows** uses PowerShell:

```powershell
irm https://raw.githubusercontent.com/easelian/remote-device-agent/main/packaging/install.ps1 | iex
```

The scripts select the newest published GitHub release, including prereleases,
verify the platform binary's SHA-256 and reported version, and install one file:
`~/.local/bin/rdh` on Linux/macOS or
`%LOCALAPPDATA%\RemoteDeviceAgent\rdh.exe` on Windows. Linux defaults to the musl
build. x86_64 and ARM64 artifacts are supported.

The installer saves the command directory in your user PATH and launches `rdh`.
Without a terminal, the POSIX installer finishes installation and asks you to run
`rdh` interactively.
New POSIX terminals load the saved PATH; Windows also updates the current
PowerShell session. Existing command files are preserved. Use `RDH_NO_START=1`
for installation only or `RDH_NO_MODIFY_PATH=1` to leave PATH unchanged.
`RDH_INSTALL_DIR` selects another absolute installation directory.
See [Distribution](docs/distribution.md#bootstrap-installation) for offline
artifacts, mirrors and installer options.

### Pair and connect

Run the Agent in an interactive terminal:

```sh
rdh
```

On first run, choose the directory to share. Press Enter to accept the displayed
current directory. The Agent saves this selection in local `settings.json`,
prints a pairing code and verification link, and attempts to open the browser.
Review the device, shared directory and permissions there before approving.
If the browser does not open, use the printed link.

A bound Agent reuses its saved identity and roots without opening the browser.
It stays connected in the foreground until Ctrl+C or termination. Running `rdh`
from a different directory does not change the saved shared directory.

The public commands are:

| Command | Behavior |
| --- | --- |
| `rdh` | Start the Agent; request browser approval when unbound. |
| `rdh unbind` | Stop the Agent and managed work, revoke the binding, and cancel recorded automatic startup. |
| `rdh update` | Update the executable after the Agent has stopped. |
| `rdh --help` | Show command help. |
| `rdh --version` | Show the executable version. |

### Unbind or remove the Agent

Run `rdh unbind` before removing the executable. If the Hub cannot be reached,
the Agent retains a durable unbind request and blocks reuse of the old binding.
Restore connectivity and run `rdh unbind` again. A manual `rdh` also completes
pending revocation, then exits; run it once more to pair again.

Once unbinding completes, remove only the installed executable. Keep shared
command directories and unrelated PATH entries. Removing a program file alone
does not revoke its device identity. Local shared-directory settings are retained
for a later pairing.

### Update

Finish managed work and stop the Agent, then run:

```sh
rdh update
```

The updater selects the newest published GitHub release, including prereleases,
and verifies the candidate before replacing the executable. Binding and startup
settings are preserved. Run `rdh` after a successful update to reconnect.
See [Maintenance](docs/maintenance.md#update-the-agent) for failure handling.

## Permissions, startup and network

File roots and path policies constrain file tools. Shell commands run with the
Agent OS user's permissions; sharing a directory does not confine shell access
to that directory. Review shell permission separately when approving a device.
A timeout does not prove an operation had no effect; inspect uncertain write or
process outcomes before retrying.

Automatic startup requests use the startup RPCs. Run `rdh` again in a local interactive terminal to approve or deny a pending request. Windows automatic startup uses the standard `LOCALAPPDATA` configuration directory.
Unbinding cancels startup registrations recorded for the binding. The public CLI
contains only the commands listed above.

The configuration directory is normally `~/.config/remote-device-hub` on POSIX
and `%LOCALAPPDATA%\remote-device-hub` on Windows. POSIX honors `XDG_CONFIG_HOME`.
It contains private binding state and `settings.json`; permissions are checked
before use. Keep credentials and pending lifecycle records private.

New pairings use the Hub address compiled into
[deployment_defaults.rs](src/deployment_defaults.rs). An existing binding keeps
its saved Hub. Connections are direct by default; `RDH_USE_ENV_PROXY=1` enables
local proxy variables and `RDH_CA_BUNDLE` adds PEM trust roots.
See [Network configuration](docs/network.md) and [Hardening](docs/hardening.md).

## Browser tasks

Opt-in Linux, macOS and Windows browser sessions support structured page reads, browser-only input,
human login, screenshots, named profiles and bounded file transfer. Use the
[Browser guide](docs/browser.md) to enable the local policy and check the matching
Hub requirements. Browser support is a preview feature and is disabled by
default. It requires matching Hub/MCP and Business web builds; the guide records
the validated platforms and remaining acceptance limits.

## Development and documentation

Use Rust 1.93.0 and the checked-in lockfile. Build and test instructions are in
[Maintenance](docs/maintenance.md#build-and-validate); platform execution requires
the corresponding OS, linker and SDK. Source inspection and cross-compilation do
not establish native runtime behavior or power-loss durability.

- [Code navigation](docs/code-structure.md) ([简体中文](docs/code-structure-zhCN.md)): source ownership and reading order.
- [Lifecycle](docs/native-lifecycle.md) ([简体中文](docs/native-lifecycle-zhCN.md)): startup, unbinding and executable replacement.
- [Architecture](docs/architecture.md): connections, requests and work lifetimes.
- [Distribution](docs/distribution.md): artifacts, installers and release preparation.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
Bundled dependencies retain their own licenses in each native artifact's
manifest and license directory.
