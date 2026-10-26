#!/usr/bin/env node
import { readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';

// Existing fault suites contain POSIX shells, permissions and process groups.
// Native Windows gets its own real filesystem/process/pairing acceptance suite,
// plus the platform-neutral transport, CLI, release and server-selection tests.
const windowsSuites = new Set([
  'agent-proxy.test.mjs',
  'audit.test.mjs',
  'bootstrap.test.mjs',
  'transport.test.mjs',
  'release.test.mjs',
  'server-selection.test.mjs',
  'platform-storage.test.mjs',
  'platform-runtime.test.mjs',
  'runtime-regression.test.mjs',
  'operation-receipts.test.mjs',
]);
const files = readdirSync(new URL('../test/', import.meta.url))
  .filter((name) => name.endsWith('.test.mjs'))
  .filter((name) => process.platform !== 'win32' || windowsSuites.has(name))
  .sort()
  .map((name) => `test/${name}`);
const result = spawnSync(process.execPath, ['--test', ...files], { stdio: 'inherit' });
if (result.error) {
  throw result.error;
}
process.exitCode = result.status ?? 1;
