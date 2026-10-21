> Historical validation record: versions, environments, and paths below predate the repository split. This is not acceptance evidence for current split-repository artifacts. Use the current repository README and maintenance documentation for commands.

# Local validation of Node Agent 0.5.0

The `remote-device-hub` npm Agent 0.5.0 runs directly on Node.js 22+. Its package contains no Python, uv, wheels, native `.node` addons, platform-specific prebuilt binaries, or installation hooks. WebSocket support includes only the required client source from a pinned `ws` version and its MIT license. The Hub and standalone Python Agent remain independently maintained components and do not participate in npm Agent startup.

The source baseline is `0f54e9b84cda470ccd3f7af98b704571a6441d2c`. The local release artifact is [remote-device-hub-0.5.0.tgz](../dist/remote-device-hub-0.5.0.tgz); its digest and inputs are recorded in [agent-manual-0.5.0.json](../releases/agent-manual-0.5.0.json). Status remains `prepared`: no npm publication, Git push, production deployment, or ChatGPT plugin update was performed.

## Measured results

The final artifact's SHA-256 is `5f5a5be581cc4449ea2a5fe3ec95e8d7ef5a1a0c4157ad3c11082b15cac5c3e3`.

| Measurement | Result |
| --- | --- |
| TGZ size | 43,045 bytes, approximately 42.0 KiB |
| Total unpacked file size | 169,950 bytes, approximately 166.0 KiB |
| Empty npm cache, first launch to pairing code | 688 ms |
| Cached launch to pairing code, five samples | 329 / 332 / 325 / 335 / 328 ms |
| Reusing an existing pairing to WebSocket online | 415 ms |
| First launch to online, with automatic test approval | 3,785 ms |

Tests ran on macOS arm64 with Node 22.23.3, the local TGZ, and a real local Hub fixture. npm ran with `--offline`, its registry pointed to an unreachable loopback port, and PATH contained only Node and the system `/bin/sh`. First-launch timing includes installing the local TGZ with npm and starting the Agent. It excludes downloading the TGZ over the internet and manual pairing approval. Pairing-code timing runs from child-process startup to receipt of the output line. Online timing includes the protocol's minimum three-second pairing-poll delay.

Raw nonsensitive results are in [node-agent-0.5.0-runtime.json](../releases/evidence/2026-09-30/node-agent-0.5.0-runtime.json). The test observed only `/api/pair/start`, `/api/pair/poll`, and `/agent/:device_id`. No Python, uv, runtime cache, or external npm runtime dependencies were downloaded. Configuration, tokens, pairing codes, and raw Agent logs are excluded from this report.

## Functional and maintenance validation

The real local Hub fixture verified device information, directory listing and creation, file reads and writes, SHA-256 conflicts, text replacement, file search, process output, stdin, and process termination. Restarting after Ctrl+C reused identical configuration bytes, the device ID, and the Hub's single device record. The exit path was also fixed for duplicate interrupt signals forwarded by npm. Normal shutdown cleans up tasks and profile locks.

The complete component suite passed 103 Python tests and 31 Node tests. All 31 Node 22 core tests passed on Linux x64/arm64, macOS arm64, and macOS x64 under Rosetta. Platform tests covered candidate source; the final TGZ's end-to-end fixture check ran on macOS arm64. GitHub Actions was updated to Node 22. Agent unit tests do not install Python, and manual Agent builds now include a local Hub check of the final package. Remote Actions were not run during this validation.

Runtime source is in `npm/agent/lib/`; pinned WebSocket source and provenance are in `npm/agent/vendor/ws/`. The builder packages only client files, excluding the WebSocket server used by tests. `scripts/release.py build agent` builds the Agent independently of Hub, Protocol, or Python Agent source fingerprints. Repository build and validation tools still use Python; the npm Agent runtime on target devices does not require it.

Rebuild the release and repeat local validation:

```bash
.venv/bin/python scripts/release.py build agent
.venv/bin/python scripts/check_node_agent.py \
  --artifact dist/remote-device-hub-0.5.0.tgz \
  --node /absolute/path/to/node \
  --output state/release-evidence/node-agent-runtime.json
```

## Implementation limits

Pure Node file operations retain path-boundary checks, rejection of static symbolic links, size limits, SHA-256 conflict checks, and atomic replacement, with directory-identity checks before and after each operation. They are not equivalent to anchoring directory file descriptors with `openat`. Another local process can still replace a parent directory after a check. File roots are not a security sandbox against local-process attacks. Shell commands continue to use the Agent's operating-system account permissions.

Node profiles use an atomic `mkdir` lock directory and a private PID/nonce owner file. Normal shutdown releases only the lock owned by that process. SIGKILL, power loss, and other abnormal exits may leave a lock behind; confirm the corresponding process has stopped before cleaning it up. Node locks do not interoperate with the legacy Python `flock` protocol. Stop the old Agent before migrating, then reuse its configuration. The Hub rejects duplicate connections for the same device ID.

These results do not validate production HTTPS/WSS, public OAuth, a real ChatGPT client, or reconnection of production devices. Local success is not evidence of a live deployment.
