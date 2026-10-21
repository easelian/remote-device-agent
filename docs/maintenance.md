# Agent maintenance and releases

This repository maintains only the Node Agent. Its source, tests, builder, and CI do not depend on the Server, Integrations, or Python.

With Node.js 22+, run from the repository root:

```bash
npm ci --ignore-scripts
npm run typecheck
npm test
npm run pack:check
node scripts/release.mjs build
node scripts/release.mjs check releases/remote-device-hub-0.7.0.json
```

The version comes from `package.json`; TGZ and record names are derived from it. The package remains `remote-device-hub`, with the `remote-device-hub` and `remote-device-agent` command aliases. A version must not be overwritten with different source or artifacts.

The builder uses standard Node libraries to generate deterministic tar/gzip archives containing the precompiled, minified TypeScript entry point and vendored `ws` client. The full vendor tree stays in the source repository. The TGZ budget is 64 KiB, with at most 512 KiB unpacked. The package excludes TypeScript source, tests, build scripts, the Server, Python, native addons, installation hooks, and runtime npm dependencies. When changing a vendored library, review its origin, license, SHA, and client allowlist together.

Release records include the actual Node version, Git state, source digest, builder digest, and artifact digest. The `record` command records the host, completed checks, and evidence files for actions already performed; it does not publish to npm or deploy to devices. Release stages cannot move backward. The `verified` stage requires successful package-integrity, install-cli, binding-reuse, and protocol-compatibility checks.

Cross-repository compatibility is verified by explicitly passing this TGZ to the checker in the Server repository; neither builder reads a sibling worktree. Public TLS, production OAuth, and ChatGPT tool calls require separate evidence from the real environment. The records from the repository split remain `prepared`.

Node profiles use directory locks. Stop the old Agent before migrating, then reuse the same configuration. Use explicit `--recover-lock` to recover a lock left by an abnormal exit; only a valid private lock whose PID has exited may be reclaimed. A leftover recovery guard requires manual inspection first. The legacy Python Agent and Node Agent do not share a lock protocol. Parent-directory checks in the file adapter still have a TOCTOU limitation when another local process replaces paths concurrently. Maintenance updates must not describe file roots as an operating-system sandbox.

## Current source capabilities

An ordinary network disconnect retains processes and bounded output for up to five minutes while awaiting reconnection. Revoked authorization and HTTP 401/403 are terminal: processes must be stopped and cleaned up. Background search uses `start_search`, `get_search_results`, `stop_search`, and `list_searches`. Limits are 200 results per page, a 30-second active TTL, up to five minutes of completed-task retention, two concurrent tasks, traversal depth 30, 10,000 entries visited, and at most 1,000 results or 256 KiB. `truncated`, `truncated_reason`, `skipped`, and `status` report these boundaries.

These interfaces and resource limits carry forward from `0.6.2` through `0.6.4`. Metadata is retained for at most 16 searches. Canceled workers continue to occupy concurrency slots until their file descriptors have closed. Release status is determined by this repository's release records and evidence. Python Agent `0.4.0` retains the legacy core-operation contract and explicitly rejects these extended operations.

## Local package validation on 2026-09-30

The `0.6.0` [prepared record](../releases/remote-device-hub-0.6.0.json) contains a 48,899-byte TGZ with 196,632 bytes of unpacked files. The full 59-test suite passed on both macOS arm64 and Linux x86_64 with Node `22.23.3`. After the final gzip OS-header correction, all five release tests passed on each platform. The Linux rebuild matched the official macOS artifact byte for byte.

The installed Hub `0.5.0` wheel and final TGZ were tested for pairing, core and extended tools, searches, process operations, and reconnection with the same configuration. A job retained its PID across a WebSocket disconnect, and stdin and output remained usable after reconnection. This cross-package check ran on macOS arm64. Evidence: [tests](../releases/evidence/node-0.6.0-tests-20260930.json), [Linux rebuild](../releases/evidence/linux-0.6.0-20260930.json), and [Server compatibility](../releases/evidence/server-compat-0.6.0-20260930.json).

These results establish local and physical Linux-host behavior. They do not establish npm publication, production deployment, or acceptance in a real ChatGPT client. Artifact status remains `prepared`. CI is configured for Linux/macOS with Node 22, but remote GitHub Actions runs were not executed in this validation batch.

## Quality gates for 0.6.2

Updates include asynchronous file I/O, segmented output buffers, a bounded WebSocket send queue, operation budgets that survive reconnection, and explicit lock recovery. `check_stability.mjs` repeatedly exercises files, searches, and processes over a real loopback WebSocket, checking output limits, network reconnection, and actual resource cleanup. `check_lifecycle.mjs --full` uses real elapsed time to test 1/10/60/299/301-second offline samples and permanent authorization denial. Test scripts are excluded from the TGZ.

A candidate must not be marked production-ready before completing a 24-hour soak, production WSS/OAuth validation, and real ChatGPT acceptance. Server results for 1,000 online devices and 100 concurrent operations must be recorded separately from local Agent execution performance and tied to the final source and artifact SHA. Existing 0.6.0 prepared artifacts and historical evidence are retained.

## Address selection and the central workspace in 0.6.4

The maintenance directory at the time of migration was `/home/kylin/projects/one-click/remote-device-agent`. On 2026-10-13, the full Git history, uncommitted 0.6.4 changes, and release records were imported from the Mac development repository. The original directory was retained as a backup; subsequent development moved here. See the [Server synchronization record](../../remote-device-server/docs/cross-thread-sync-20261013.md) for provenance and local acceptance checks.

Without `--server`, startup first probes `https://xdistribute.site:8090/healthz`. Connection setup, TLS, and response-body reads share a 500 ms deadline. Failure or timeout selects `https://mcp.xdistribute.site`; an explicit `--server` fixes the endpoint. This applies only to startup address selection. Runtime disconnects continue to use the existing WebSocket reconnection mechanism.

The two fixed HTTPS origins reuse an existing pairing and jointly hold locks for the profile's old and new paths. Pairing URLs and returned configurations may treat them as aliases. Custom Hubs retain same-origin validation, and credentials are never reused with another Hub. Connections use the selected endpoint; existing on-disk pairing data remains unchanged.

The 0.6.4 release TGZ and source fingerprint are frozen. Its README retains the 0.6.2 wording used at publication. Updating packaging inputs requires a version increase; published files must not be replaced under the same version. That synchronization updated only maintenance documentation outside the package and preserved each original release record's stage. Imported evidence records historical production actions; the local checks do not imply another deployment.
