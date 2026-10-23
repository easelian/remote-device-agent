# Agent maintenance and releases

This repository maintains the Node Agent. Its source, tests, builder, and CI do not depend on a Server checkout, integrations, or Python. Use Node.js 22 or later for development.

## Build and validate

Run from the repository root:

```sh
npm ci --ignore-scripts
npm run style:check
npm run typecheck
npm test
npm run pack:check
```

`npm test` builds the TypeScript modules before running the tests. `npm run pack:check` checks the package manifest, size limits, offline installation with an empty cache, and both CLI aliases. Tests are selected for the host platform; a skipped platform-specific test is not validation of that platform.

On Linux/macOS, run the local connection and resource-cleanup probes after building:

```sh
node scripts/check_stability.mjs --duration-seconds 30 --json
node scripts/check_lifecycle.mjs --quick
```

These probes use a temporary root and a real loopback WebSocket peer. The stability probe exercises files, searches, processes, reconnection, output limits, and cleanup. The lifecycle probe's `--quick` mode checks a short disconnect and authorization rejection; its default mode, also selected by `--full`, covers longer disconnects around the five-minute grace period using real elapsed time. The default stability-probe duration is 24 hours when no duration is supplied.

Local checks do not establish public WSS/OAuth behavior or acceptance in a real client. Validate those paths separately before declaring a deployment ready. Cross-repository compatibility checks must use the exact candidate TGZ and the intended Server version; the Agent builder does not read a sibling worktree.

## Package constraints

`package.json` supplies the version. The package name is `remote-device-hub`, with `remote-device-hub` and `remote-device-agent` command aliases. The builder uses Node's standard libraries to create deterministic tar/gzip archives from the compiled bundle, CLI entry point, README, package metadata, and third-party licenses.

The npm TGZ budget is 64 KiB, with at most 512 KiB unpacked. TypeScript source, tests, build scripts, runtime npm dependencies, native addons, and installation hooks are excluded. The full vendor tree remains in the source repository. Review a vendored library's origin, license, digest, and client allowlist together when changing it.

## Prepare and publish a release

After updating `package.json` and completing validation, build the local TGZ and check its generated record:

```sh
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.7.0.json
```

Use the version from `package.json` in the check path. Release records bind the Node version, Git state, source and builder digests, and artifact digest. Stale compiled code or a modified bundle prevents record generation. The `record` command describes actions already completed; it does not perform publication or deployment. A published version must not be replaced with different source or artifacts.

Pushing a matching `v*` tag runs the release workflow, which builds runtime archives, verifies their checksums, performs a Linux installation smoke test, and publishes GitHub Release assets. Channel promotion is a separate workflow that changes rollout metadata without rebuilding artifacts. See [Distribution](distribution.md) and the [rollout instructions](../README.md#releases-and-staged-rollouts).

## Recover a profile lock

Stop the old Agent before starting another process with the same configuration. Use `--recover-lock` only after confirming that the previous process has exited. Recovery accepts a valid private lock belonging to an exited PID; a leftover recovery guard requires manual inspection. See [Architecture](architecture.md) for shutdown and resource-lifetime behavior.
