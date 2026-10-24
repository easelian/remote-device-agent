import fs from 'node:fs';
import path from 'node:path';
import { createInterface } from 'node:readline/promises';
import { parseArgs } from './bootstrap.js';
import { FALLBACK_SERVER } from './server-selection.js';
import { configPath, readConfig } from './config.js';
import { NativeStartup, hasAdministratorAuthority } from './startup-native.js';
import { Startup, startupMode, setManagedStartupGrant } from './startup.js';

async function confirm(message: string) {
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error('Local confirmation requires an interactive terminal');
  }
  const terminal = createInterface({ input: process.stdin, output: process.stdout });
  try {
    if ((await terminal.question(`${message}\nType yes to confirm: `)).trim() !== 'yes') {
      throw new Error('Startup change cancelled');
    }
  } finally {
    terminal.close();
  }
}
export async function runStartup(argv: string[]) {
  const [command = 'status', ...rest] = argv;
  if (command === 'system') {
    const action = rest.shift() || 'status';
    const mode = action === 'set' ? startupMode(rest.shift()) : null;
    let name =
      process.platform === 'win32'
        ? 'RemoteDeviceHub'
        : process.platform === 'darwin'
          ? 'com.be-one-click.rdh'
          : 'rdh.service';
    if (rest[0] === '--name' && rest.length === 2) {
      name = rest[1] || '';
      rest.length = 0;
    }
    if (
      !['status', 'set'].includes(action) ||
      rest.length ||
      !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,99}$/.test(name) ||
      mode === 'user'
    ) {
      throw new Error('Use startup system status|set none|service [--name NAME]');
    }
    const backend = new NativeStartup(path.resolve('.'));
    const binding = { mode: 'service' as const, name };
    if (mode !== null) {
      if (!hasAdministratorAuthority()) {
        throw new Error('Run this command as an administrator; automatic elevation is disabled');
      }
      if (mode === 'service') {
        await confirm(
          `Enable the existing boot service ${name}? Its current process will remain running.`,
        );
      }
      backend.set(binding, mode === 'service');
    }
    console.log(
      JSON.stringify({
        mode: backend.enabled(binding) ? 'service' : 'none',
        registration: binding,
        current_connection: 'unchanged',
      }),
    );
    return;
  }
  if (command === '--help' || command === '-h') {
    console.log(`rdh startup status [--config PATH | --server URL --profile NAME]
rdh startup set none|user|service [--config PATH]
rdh startup approve REQUEST_ID [--config PATH]
rdh startup deny REQUEST_ID [--config PATH]
rdh startup grant user|service [--config PATH]  (administrator; managed devices)
rdh startup revoke [--config PATH]             (administrator)
rdh startup bind user|service NAME [--config PATH] (existing installer registration)
rdh startup system status|set none|service [--name NAME] (existing boot service; administrator to change)

Enabling requires interactive local confirmation or an administrator grant for remote requests.
Changes affect future startup only; they do not stop or start the current Agent.
An unbound profile reports unknown; bind an older installation after inspecting its native registration.
Boot service installation/account migration remains an explicit administrator operation.
Shell commands run as the Agent OS user and are not constrained by this approval interface.`);
    return;
  }
  if (!['status', 'set', 'approve', 'deny', 'grant', 'revoke', 'bind'].includes(command)) {
    throw new Error('Unknown startup command');
  }
  const value = ['set', 'approve', 'deny', 'grant', 'bind'].includes(command)
    ? rest.shift()
    : undefined;
  const name = command === 'bind' ? rest.shift() : undefined;
  const options = parseArgs(rest);
  let candidate = options.config || configPath(options.server, options.profile);
  if (
    !options.config &&
    !rest.some((arg) => arg === '--server' || arg.startsWith('--server=')) &&
    !fs.existsSync(candidate)
  ) {
    candidate = configPath(FALLBACK_SERVER, options.profile);
  }
  const config = fs.realpathSync.native(path.resolve(candidate));
  if (command === 'grant' || command === 'revoke') {
    const mode = command === 'revoke' ? 'none' : startupMode(value);
    // This explicit command writes an administrator-only policy; OS elevation
    // must already have been granted by the caller, including managed deployment.
    setManagedStartupGrant(config, mode);
    console.log(JSON.stringify({ status: 'applied', managed_mode: mode }));
    return;
  }
  await readConfig(config);
  const startup = new Startup(config);
  let result;
  if (command === 'status') {
    result = startup.status();
  } else if (command === 'bind') {
    const mode = startupMode(value);
    if (mode === 'none' || !name) {
      throw new Error('bind requires user|service and a native registration name');
    }
    result = await startup.bind(mode, name);
  } else if (command === 'set') {
    const mode = startupMode(value);
    if (mode !== 'none') {
      await confirm(
        `Enable ${mode} startup for ${config}? The current connection will remain running.`,
      );
    }
    result = await startup.set(mode, true);
  } else {
    if (!value) {
      throw new Error('An approval request ID is required');
    }
    if (command === 'approve') {
      const pending = startup.status().pending;
      if (!pending || pending.request_id !== value) {
        throw new Error('No matching pending startup request');
      }
      await confirm(
        `Approve remote request ${value}: enable ${pending.mode} startup for ${config}?`,
      );
    }
    result = await startup.approve(value, command === 'approve');
  }
  console.log(JSON.stringify(result, null, 2));
}
