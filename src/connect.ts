import * as fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { createHash, randomBytes } from 'node:crypto';
import { spawn } from 'node:child_process';
import {
  AgentConfig,
  configPath,
  configPaths,
  localRoots,
  readConfig,
  validateGateway,
  writeConfig,
} from './config.js';
import { acquireProfileLocks, reclaimProfileLocks } from './profile-lock.js';
import { profileServers, sameHub } from './server-selection.js';
import { defaultShell, platformName, ensurePrivatePath } from './platform.js';
import type { LaunchArgs, DeviceConfig } from './types.js';
import { errorCode, isRecord, type Awaitable } from './boundary.js';

interface ReleasableLock {
  release(): Awaitable<void>;
}
export interface LockProvider<Handle> {
  acquire(paths: string[]): Awaitable<Handle>;
  release?(handle: Handle): Awaitable<void>;
  reclaim?(paths: string[]): Awaitable<unknown>;
}
export interface ConnectOptions<Result, Handle> {
  locks?: LockProvider<Handle>;
  runAgent?(
    config: DeviceConfig,
    signal: AbortSignal | undefined,
    options: { configFile: string },
  ): Awaitable<Result>;
  canReleaseLocks?(): Awaitable<boolean>;
}
function hasRelease(value: unknown): value is ReleasableLock {
  return isRecord(value) && typeof value.release === 'function';
}

async function acquireLocks<Handle>(
  paths: string[],
  provider?: LockProvider<Handle>,
): Promise<() => Awaitable<void>> {
  if (!provider) {
    const handle = await acquireProfileLocks(paths);
    return () => handle.release();
  }
  const handle = await provider.acquire(paths);
  if (hasRelease(handle)) {
    return () => handle.release();
  }
  const release = provider.release;
  if (!release) {
    throw new Error('Injected lock provider must supply a release operation');
  }
  return () => release.call(provider, handle);
}

const sleep = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((resolve, reject) => {
    if (signal?.aborted) {
      return reject(signal?.reason || new Error('Operation canceled'));
    }
    let onAbort: (() => void) | undefined;
    const timer = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort!);
      resolve();
    }, ms);
    onAbort = () => {
      clearTimeout(timer);
      reject(signal?.reason || new Error('Operation canceled'));
    };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
const sameRoots = (left: Record<string, string>, right: Record<string, string>) => {
  const lk = Object.keys(left).sort();
  const rk = Object.keys(right).sort();
  return (
    lk.length === rk.length &&
    lk.every((key, index) => key === rk[index] && left[key] === right[key])
  );
};

function requestSignal(signal?: AbortSignal) {
  return AbortSignal.any(
    signal ? [signal, AbortSignal.timeout(20_000)] : [AbortSignal.timeout(20_000)],
  );
}

export async function pairDevice(
  server: string,
  roots: Record<string, string>,
  name: string,
  { openBrowser = true, signal }: { openBrowser?: boolean; signal?: AbortSignal | undefined } = {},
): Promise<DeviceConfig> {
  const token = randomBytes(32).toString('base64url');
  const tokenHash = createHash('sha256').update(token).digest('hex');
  const proposal = {
    name,
    hostname: os.hostname() || 'device',
    os: platformName(),
    machine: os.machine(),
    roots,
    token_hash: tokenHash,
  };
  const origin = new URL(server);
  const post = async (pathname: string, body: Record<string, unknown>) =>
    fetch(new URL(pathname, origin), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
      redirect: 'error',
      signal: requestSignal(signal),
    });
  const response = await post('/api/pair/start', proposal);
  if (response.status !== 201) {
    throw new Error(`The Hub could not create a pairing request (HTTP ${response.status})`);
  }
  const pending: unknown = await response.json();
  if (
    !isRecord(pending) ||
    typeof pending.verification_uri_complete !== 'string' ||
    typeof pending.user_code !== 'string'
  ) {
    throw new Error('The Hub returned an invalid pairing URL');
  }
  const verification = new URL(pending.verification_uri_complete);
  if (
    !sameHub(verification.origin, origin.origin) ||
    verification.username ||
    verification.password ||
    verification.hash ||
    verification.pathname !== '/admin/pair' ||
    verification.searchParams.get('code') !== pending.user_code ||
    [...verification.searchParams.keys()].length !== 1
  ) {
    throw new Error('The Hub returned an invalid pairing URL');
  }
  if (
    typeof pending.device_code !== 'string' ||
    !pending.device_code ||
    typeof pending.expires_in !== 'number' ||
    !Number.isFinite(pending.expires_in) ||
    pending.expires_in <= 0 ||
    typeof pending.interval !== 'number' ||
    !Number.isFinite(pending.interval)
  ) {
    throw new Error('The Hub returned an invalid pairing response');
  }
  console.log(`\nPairing code: ${pending.user_code}\nOpen: ${verification.href}\n`);
  console.log(
    'Check the device and pairing code in your browser, choose permissions, and approve pairing. The code expires in 10 minutes.',
  );
  if (
    openBrowser &&
    !process.env.SSH_CONNECTION &&
    (['darwin', 'win32'].includes(process.platform) ||
      process.env.DISPLAY ||
      process.env.WAYLAND_DISPLAY)
  ) {
    const command =
      process.platform === 'win32'
        ? path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'rundll32.exe')
        : process.platform === 'darwin'
          ? 'open'
          : 'xdg-open';
    const browserArgs =
      process.platform === 'win32'
        ? ['url.dll,FileProtocolHandler', verification.href]
        : [verification.href];
    const child = spawn(command, browserArgs, {
      detached: true,
      stdio: 'ignore',
      windowsHide: true,
    });
    child.on('error', () => {});
    child.unref();
  }
  const deadline = Date.now() + Math.min(Number(pending.expires_in), 600) * 1000;
  const interval = Math.max(3, Math.min(Number(pending.interval), 10)) * 1000;
  while (Date.now() < deadline) {
    await sleep(interval, signal);
    let result;
    try {
      result = await post('/api/pair/poll', { device_code: pending.device_code });
    } catch (error) {
      if ((isRecord(error) && error.name === 'AbortError') || signal?.aborted) {
        throw error;
      }
      continue;
    }
    if (result.status === 200) {
      const body: unknown = await result.json();
      if (!isRecord(body) || !isRecord(body.config)) {
        throw new Error('Invalid device configuration');
      }
      // Older Hubs supply /bin/sh as a schema default even for Windows roots.
      const model = AgentConfig.validate({
        ...body.config,
        token,
        ...(process.platform === 'win32' && body.config.shell === '/bin/sh'
          ? { shell: defaultShell() }
          : {}),
      });
      if (!sameHub(model.gateway_url, server) || !sameRoots(model.roots, roots)) {
        throw new Error('The pairing response does not match the requested Hub or file roots');
      }
      return model;
    }
    if (result.status === 202 || result.status === 429) {
      continue;
    }
    if (result.status === 403) {
      throw new Error('Pairing was denied or revoked. Run the command again.');
    }
    if (result.status === 410) {
      break;
    }
    throw new Error(`Pairing request failed (HTTP ${result.status})`);
  }
  throw new Error('Pairing timed out. Run the command again to get a new code.');
}

export async function connectDevice<Result = void, Handle = unknown>(
  args: LaunchArgs,
  { locks, runAgent, canReleaseLocks = () => true }: ConnectOptions<Result, Handle> = {},
) {
  const signal = args.signal;
  if (!['linux', 'darwin', 'win32'].includes(process.platform)) {
    throw new Error('Only Windows, Linux, and macOS are supported');
  }
  const server = validateGateway(args.server);
  const profile = args.profile || 'default';
  const servers = profileServers(server);
  // Keep both entrances under the same lock set and reuse an existing binding
  // when startup chooses a different route. Credentials never cross custom hubs.
  let destination = args.config
    ? path.resolve(args.config.replace(/^~(?=\/|$)/, os.homedir()))
    : configPath(servers[0], profile);
  if (!args.config) {
    for (const candidate of servers.map((item) => configPath(item, profile))) {
      try {
        await fs.lstat(candidate);
        destination = candidate;
        break;
      } catch (error) {
        if (errorCode(error) !== 'ENOENT') {
          throw error;
        }
      }
    }
  }
  const lockTargets = args.config
    ? [destination]
    : servers.flatMap((item) => configPaths(item, profile));
  for (const target of lockTargets) {
    if (process.platform === 'win32') {
      ensurePrivatePath(path.dirname(target), true);
    } else {
      await fs.mkdir(path.dirname(target), { recursive: true, mode: 0o700 });
    }
  }
  if (args.recoverLock) {
    if (locks?.reclaim) {
      await locks.reclaim(lockTargets);
    } else {
      await reclaimProfileLocks(lockTargets);
    }
  }
  const releaseLocks = await acquireLocks(lockTargets, locks);
  try {
    let config;
    try {
      config = await readConfig(destination);
      if (!sameHub(config.gateway_url, server)) {
        throw new Error(
          'The existing configuration belongs to another Hub. Choose a different --profile.',
        );
      }
      if (args.root && !sameRoots(config.roots, await localRoots(args.root))) {
        throw new Error(
          'This profile is paired with different file roots. Use --profile to create another profile.',
        );
      }
      console.log(`Using saved pairing: ${config.device_id}`);
    } catch (error) {
      if (errorCode(error) !== 'ENOENT') {
        throw error;
      }
      const roots = await localRoots(args.root);
      console.log(`Target device: ${os.hostname() || 'device'}`);
      console.log(`File roots: ${Object.values(roots).join(', ')}`);
      config = await pairDevice(server, roots, args.name || os.hostname() || 'device', {
        openBrowser: !args.noBrowser,
        signal,
      });
      await writeConfig(destination, config);
      console.log(`Device paired: ${config.device_id}\nConfiguration saved to: ${destination}`);
    }
    if (args.pairOnly) {
      console.log(`Pairing complete. Configuration saved to: ${destination}`);
      return config;
    }
    console.log('Connecting to the Hub. Keep this terminal open; press Ctrl+C to stop.');
    return runAgent
      ? await runAgent({ ...config, gateway_url: server }, signal, { configFile: destination })
      : undefined;
  } finally {
    // Unfinished I/O must not overlap a second Agent using this profile. A
    // forced exit leaves a stale lock for explicit recovery after inspection.
    if (await canReleaseLocks()) {
      await releaseLocks();
    }
  }
}
