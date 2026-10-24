import * as fs from 'node:fs/promises';
import fsSync from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import type { DeviceConfig } from './types.js';
import { errorCode, isInteger, isRecord } from './boundary.js';
import {
  configBase,
  defaultShell,
  absoluteDeviceRoot,
  assertPrivatePath,
  ensurePrivatePath,
  secureNewPath,
} from './platform.js';

const PROFILE_RE = /^[A-Za-z0-9_-]{1,64}$/;
const DEVICE_RE = /^[A-Za-z0-9_-]{1,64}$/;

export function validateGateway(value: string): string {
  const url = new URL(value);
  const loopback = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname);
  if (
    value !== value.trim() ||
    /[\\\u0000-\u0020]/u.test(value) ||
    value.includes('?') ||
    value.includes('#') ||
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== '/' ||
    (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback))
  ) {
    throw new Error('--server must be an HTTPS Hub URL (HTTP is allowed for local debugging)');
  }
  return url.origin;
}

export class AgentConfig {
  static validate(value: unknown): DeviceConfig {
    if (!isRecord(value)) {
      throw new Error('Invalid device configuration');
    }
    const keys = new Set([
      'gateway_url',
      'device_id',
      'token',
      'roots',
      'allow_shell',
      'allow_write',
      'shell',
      'max_processes',
      'max_process_seconds',
    ]);
    for (const key of Object.keys(value)) {
      if (!keys.has(key)) {
        throw new Error(`Unknown device configuration field: ${key}`);
      }
    }
    if (
      typeof value.gateway_url !== 'string' ||
      typeof value.device_id !== 'string' ||
      typeof value.token !== 'string' ||
      typeof value.roots !== 'object' ||
      value.roots === null
    ) {
      throw new Error('Invalid device configuration field type');
    }
    const gateway_url = validateGateway(value.gateway_url);
    const { device_id, token } = value;
    const allow_shell = value.allow_shell ?? false;
    const allow_write = value.allow_write ?? false;
    const shell = value.shell ?? defaultShell();
    const max_processes = value.max_processes ?? 4;
    const max_process_seconds = value.max_process_seconds ?? 3600;
    if (!DEVICE_RE.test(device_id)) {
      throw new Error('Invalid device ID');
    }
    if (token.length < 32) {
      throw new Error('Invalid device token');
    }
    if (!isRecord(value.roots) || !Object.keys(value.roots).length) {
      throw new Error('Invalid device roots');
    }
    const roots: Record<string, string> = Object.fromEntries(
      Object.entries(value.roots).map(([name, root]) => {
        if (!PROFILE_RE.test(name) || typeof root !== 'string' || !absoluteDeviceRoot(root)) {
          throw new Error('Invalid device roots');
        }
        return [name, root] as const;
      }),
    );
    if (
      typeof allow_shell !== 'boolean' ||
      typeof allow_write !== 'boolean' ||
      typeof shell !== 'string' ||
      !isInteger(max_processes) ||
      max_processes < 1 ||
      max_processes > 16 ||
      !isInteger(max_process_seconds) ||
      max_process_seconds < 1 ||
      max_process_seconds > 86400
    ) {
      throw new Error('Invalid device configuration parameters');
    }
    return {
      gateway_url,
      device_id,
      token,
      roots,
      allow_shell,
      allow_write,
      shell,
      max_processes,
      max_process_seconds,
    };
  }
}

export function configPath(server: string, profile = 'default', env = process.env) {
  const [current, legacy] = configPaths(server, profile, env);
  if (existsNoFollow(current)) {
    return current;
  }
  if (existsNoFollow(legacy)) {
    return legacy;
  }
  return current;
}

export function configPaths(
  server: string,
  profile = 'default',
  env = process.env,
): [string, string] {
  if (!PROFILE_RE.test(profile)) {
    throw new Error("Profile must contain only letters, numbers, '-' or '_'");
  }
  const base = configBase(env);
  const filename = `${createHash('sha256').update(server).digest('hex').slice(0, 16)}-${profile}.json`;
  const current = path.join(base, 'remote-device-hub', filename);
  const legacy = path.join(base, 'remote-device-host', filename);
  return [current, legacy];
}

function existsNoFollow(filename: string) {
  try {
    fsSync.lstatSync(filename);
    return true;
  } catch (error) {
    if (errorCode(error) === 'ENOENT') {
      return false;
    }
    throw error;
  }
}

export async function localRoots(items?: string[]): Promise<Record<string, string>> {
  const entries: [string, string][] = items?.length
    ? items.map((item): [string, string] => {
        const index = item.indexOf('=');
        if (index <= 0) {
          throw new Error('Each --root must use NAME=PATH');
        }
        return [item.slice(0, index), item.slice(index + 1)];
      })
    : [['work', process.cwd()]];
  const result: Record<string, string> = {};
  for (const [name, value] of entries) {
    if (!PROFILE_RE.test(name)) {
      throw new Error('Invalid file root name');
    }
    const resolved = await fs.realpath(path.resolve(value));
    const stat = await fs.stat(resolved);
    if (!stat.isDirectory()) {
      throw new Error('Each file root must be an existing directory');
    }
    result[name] = resolved;
  }
  return result;
}

export async function readConfig(filename: string) {
  assertPrivatePath(filename, false);
  const handle = await fs.open(
    filename,
    fsSync.constants.O_RDONLY | (fsSync.constants.O_NOFOLLOW || 0),
  );
  try {
    const stat = await handle.stat();
    if (!stat.isFile()) {
      throw new Error('The device configuration must be owned by the current user with mode 0600');
    }
    const value: unknown = JSON.parse(await handle.readFile('utf8'));
    return AgentConfig.validate(value);
  } finally {
    await handle.close();
  }
}

export async function writeConfig(filename: string, config: DeviceConfig) {
  if (process.platform === 'win32') {
    ensurePrivatePath(path.dirname(filename), true);
  }
  const data = `${JSON.stringify(AgentConfig.validate(config), null, 2)}\n`;
  const handle = await fs.open(filename, 'wx', 0o600);
  try {
    secureNewPath(filename, false);
    await handle.writeFile(data, 'utf8');
  } finally {
    await handle.close();
  }
}
