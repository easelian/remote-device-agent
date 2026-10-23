# Remote Device Hub Agent

A device Agent written in TypeScript. Native Windows, Linux, and macOS share the CLI, pairing protocol, and file and process interfaces. The npm package requires Node.js 22+; `rdh-*.tar.gz` release archives include the runtime and do not require Node.js to be preinstalled.

The npm package is `remote-device-hub`, with two command aliases: `remote-device-agent` and `remote-device-hub`. The final package contains only bundled, minified JavaScript, the CLI entry point, this README, and third-party licenses. It has no runtime npm dependencies, native addons, installation scripts, first-run compilation, or additional runtime downloads. TypeScript, type declarations, and build tools are development dependencies and are excluded from the release package.

## Build and validate

Run from the repository root:

```sh
npm ci --ignore-scripts
npm run typecheck
npm test
npm run pack:check
node scripts/release.mjs build
```

The release builder derives artifact and record names from `package.json`. Verify a generated record by passing its path to `node scripts/release.mjs check`.

Source files live in `src/*.ts`. `npm run build` generates modules in `.build/` for tests and `bundle/agent.mjs` for distribution. `npm pack` runs type checking and compilation first. The release TGZ must be no larger than 64 KiB, with at most 512 KiB of unpacked content. Package checks also verify the file manifest, offline installation with an empty cache, and both command aliases. Stale compiled code or a modified bundle prevents release-record generation.

Building writes local files only. It does not publish to npm, deploy to devices, or link an account. See [Maintenance and releases](docs/maintenance.md) for build checks and the release workflow.

## Documentation

- [Architecture](docs/architecture.md): component responsibilities, connection flow, and operation lifetimes.
- [Hardening configuration](docs/hardening.md): operation policies, sandboxing, environment inheritance, and local auditing.
- [Distribution](docs/distribution.md): runtime archives, npm packages, and internal mirrors.
- [Maintenance and releases](docs/maintenance.md): development checks, package constraints, and release preparation.

## Install and run

Choose the installation method that matches the device:

| Method | Node.js requirement | Platforms | How the Agent runs |
| --- | --- | --- | --- |
| Runtime installer | Node.js is included | Linux / macOS | Registered user service |
| `npx` | Node.js 22+ and npm installed on the device | Windows / Linux / macOS | In the current terminal |

### Option 1: Install with Node.js included

The installer includes the Node runtime, so devices do not need a preinstalled Node.js runtime or root access:

```sh
curl -fsSL https://github.com/be-one-click/remote-device-agent/releases/latest/download/install.sh | sh
```

The script detects the platform (glibc/musl and x64/arm64), verifies `SHA256SUMS`, extracts the release to `~/.local/lib/rdh/versions/<version>`, switches the `current` symlink, completes pairing in a browser, and registers a systemd user service (launchd on macOS). Run it again to upgrade.

Installer options:

- `--server URL`: use a specific Hub URL.
- `--root NAME=PATH`: expose a file root; repeat for multiple roots.
- `--no-pair`: install only; run `rdh --pair-only` later to pair.
- `--no-service`: install without registering a service.
- `--offline FILE`: install from a local archive.
- `--uninstall`: remove the installation while retaining configuration and audit logs.

If a device cannot reach GitHub, use `--base` to select an internal mirror with the same directory layout as the release page:

```sh
sh install.sh --base https://mirror.example.com/rdh
```

### Option 2: Run with npx using the device's Node.js

Install Node.js 22+ and npm on the device first. This method uses that Node.js runtime; the Agent package does not include another copy. `npx` downloads the package into npm's cache when needed and runs it without a global installation.

Run the latest release from your configured npm registry:

```sh
npx --yes remote-device-hub@latest
```

The registry command requires the package to be published with a `latest` tag. For a TGZ distributed directly, save the downloaded package as `remote-device-hub.tgz` in the current directory and run:

```sh
npx --yes --package=./remote-device-hub.tgz remote-device-agent
```

Both commands start the Agent in the foreground. Keep the terminal open; press Ctrl+C to stop it. Add `--version` to check the package version, or `--no-browser` to print the pairing URL without opening a browser. Use the same command again to reuse the saved pairing.

### Pairing and file roots

On first run, the Agent prints a pairing URL and code. Check the device and permissions on the page before approving. The current directory is the default file root.

Use `--root NAME=PATH` to configure roots; repeat it as needed and quote the entire argument when a path contains spaces. `--profile NAME` creates a separate pairing, and `--server URL` selects a specific Hub. With `npx`, place these Agent options after the package name or explicit `remote-device-agent` command. Windows uses native drive-letter or UNC roots; relative paths in file tools still use `/`. Windows pairing requires a Hub with Protocol 1.0.1 or a newer compatible version. Older server models reject Windows pairing proposals.

## Releases and staged rollouts

Pushing a `v*` tag triggers CI to build and publish runtime archives for six platforms. No manual upload is required. Publishing runtime archives and publishing the npm package to a registry are separate distribution steps.

Publishing a version and moving a channel pointer are separate steps. Expanding a rollout does not require rebuilding the package:

1. **Prerelease:** set the intended prerelease version in `package.json` and create a matching tag with a `v` prefix. CI builds and publishes a GitHub Release marked as a prerelease.
2. **Canary:** in Actions, run `Promote` with `channel=canary`, set `version` to the published prerelease, and use `percent=100`. Devices that opt in run `rdh upgrade --channel canary`.
3. **Stable release:** set the intended stable version in `package.json` and create its matching `v` tag.
4. **Staged rollout:** run `Promote` with `channel=stable`, set `version` to the published stable release, and use `percent=10`. A hash of the `install-id` generated during installation selects approximately 10% of devices. Run it again with `percent=50` or `100` to expand the rollout. Bucket assignments remain stable.
5. **Rollback:** point `stable` to the previous version by running `Promote` again, or run `rdh rollback` on a device.

Check or upgrade on a device:

```sh
rdh upgrade --check                 # Report only; make no changes
rdh upgrade                         # Follow the stable channel and rollout percentage
rdh upgrade --channel canary        # Opt in to canary
rdh rollback                        # Restore the previous version
```

The default channel index is `channels/<channel>.json` in this repository. Use `--index` for an internal mirror:

```sh
rdh upgrade --index https://mirror.example.com/rdh/stable.json
```

The index contains the version, `min_from` (which prevents unsupported version jumps), and `rollout.percent`. Each upgrade extracts into a new directory, verifies `--version`, switches `current`, and restarts the service. A failed health check triggers an automatic rollback.

## Platform behavior and limits

| Capability | Linux / macOS | Windows |
| --- | --- | --- |
| Pairing, files, search, and processes | Shared interfaces | Shared interfaces, with drive-letter, reserved-name, and alternate-data-stream validation |
| Default shell | `/bin/sh` | System `cmd.exe`; commands must use the target system's syntax |
| Private configuration and audit logs | Owned by the current user; files `0600`, directories `0700` | Private ACL owned by the current user with inheritance disabled; verified using system PowerShell |
| Process termination | POSIX process groups | Best-effort termination of a live command tree with `taskkill /T /F`; detached descendants are not guaranteed to be cleaned up after the parent exits |
| bubblewrap sandbox | Linux only; bwrap must be installed separately | Unsupported; configuring it prevents startup |
| Shell resource limits | Applied when supported by the shell; failure stops execution | POSIX ulimit is unsupported; configuring it prevents startup |

The default configuration directory is `~/.config/remote-device-hub` on Linux/macOS and `%LOCALAPPDATA%\remote-device-hub` on Windows. Override it with `XDG_CONFIG_HOME` or `--config`. Existing configuration with insufficient privacy protections is not automatically repaired or made less restrictive. Local audit logging is enabled by default and retains at most the latest 1,000 JSONL operation records per device across restarts. Logs live under `~/.local/state/remote-device-hub/audit/` on Linux/macOS (`XDG_STATE_HOME` overrides the state base) or `%LOCALAPPDATA%\remote-device-hub\audit\` on Windows. See [Hardening configuration](docs/hardening.md) for record fields, redaction, retention, and the explicit opt-out.

The same guide documents explicit application environment inheritance, audit health and required audit writes, character-safe UTF-8/UTF-16 text reads, and recursive parent-directory creation. Process launch errors retain bounded OS diagnostics, and local audit records also track final process status.

File roots and path policies constrain the Agent's file tools; they are not an operating-system sandbox. Concurrent path replacement by another local process remains a TOCTOU limitation. Shell commands run as the Agent's operating-system user and are not constrained by file path policies.

Brief network interruptions retain processes and bounded output for up to five minutes. Revoked authorization, prolonged disconnection, or an explicit exit triggers cleanup. If a timeout or disconnect leaves an operation's outcome unknown, inspect its state before retrying. On Linux/macOS, `cleanup_pending` means supervisor resources have not finished closing. On Windows, this field does not prove that all detached descendants have exited. Recover a configuration lock left by an abnormal exit with `--recover-lock` only after confirming that the previous process has stopped.

CI is configured for all three operating systems, including native Windows file, ACL, pairing, and process cases. Validate changes on the affected operating system; pure path tests on Linux do not establish Windows ACL, shell, or process behavior.

## Code style

`npm run format` formats maintained source and tests. `npm run style:check` checks formatting and lint rules; `npm run lint` runs only the linter. Biome is pinned as a development dependency. Use two-space indentation, single quotes, semicolons, trailing commas, one statement per line, and explicit type imports. Explicit `any` and `@ts-ignore` are prohibited. Generated files and third-party code are excluded from formatting.
