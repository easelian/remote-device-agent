import * as fs from 'node:fs/promises';
import fsSync from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import type { DeviceConfig } from './types.js';
import { configBase, defaultShell, absoluteDeviceRoot, assertPrivatePath, ensurePrivatePath, secureNewPath } from './platform.js';

const PROFILE_RE = /^[A-Za-z0-9_-]{1,64}$/;
const DEVICE_RE = /^[A-Za-z0-9_-]{1,64}$/;

export function validateGateway(value: string): string {
  const url = new URL(value);
  const loopback = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname);
  if (url.username || url.password || url.search || url.hash || url.pathname !== '/' ||
      (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback))) {
    throw new Error('--server 必须是 HTTPS 中心地址（本机调试可使用 HTTP）');
  }
  return url.origin;
}

export class AgentConfig {
  static validate(value: any): DeviceConfig {
    if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('设备配置格式无效');
    const keys = new Set(['gateway_url', 'device_id', 'token', 'roots', 'allow_shell', 'allow_write',
      'shell', 'max_processes', 'max_process_seconds']);
    for (const key of Object.keys(value)) if (!keys.has(key)) throw new Error(`设备配置包含未知字段：${key}`);
    if (typeof value.gateway_url !== 'string' || typeof value.device_id !== 'string' ||
        typeof value.token !== 'string' || typeof value.roots !== 'object' || value.roots === null)
      throw new Error('设备配置字段类型无效');
    const result = {
      gateway_url: validateGateway(value.gateway_url),
      device_id: value.device_id, token: value.token, roots: value.roots,
      allow_shell: value.allow_shell ?? false, allow_write: value.allow_write ?? false,
      shell: value.shell ?? defaultShell(), max_processes: value.max_processes ?? 4,
      max_process_seconds: value.max_process_seconds ?? 3600,
    };
    if (!DEVICE_RE.test(result.device_id)) throw new Error('设备 ID 无效');
    if (result.token.length < 32) throw new Error('设备令牌无效');
    if (!result.roots || typeof result.roots !== 'object' || Array.isArray(result.roots) || !Object.keys(result.roots).length)
      throw new Error('设备目录无效');
    for (const [name, value] of Object.entries(result.roots)) {
      if (!PROFILE_RE.test(name) || typeof value !== 'string' || !absoluteDeviceRoot(value))
        throw new Error('设备目录无效');
    }
    if (typeof result.allow_shell !== 'boolean' || typeof result.allow_write !== 'boolean' ||
        typeof result.shell !== 'string' || !Number.isInteger(result.max_processes) || result.max_processes < 1 || result.max_processes > 16 ||
        !Number.isInteger(result.max_process_seconds) || result.max_process_seconds < 1 || result.max_process_seconds > 86400)
      throw new Error('设备配置参数无效');
    return result;
  }
}

export function configPath(server: string, profile = 'default', env = process.env) {
  const [current, legacy] = configPaths(server, profile, env);
  if (existsNoFollow(current)) return current;
  if (existsNoFollow(legacy)) return legacy;
  return current;
}

export function configPaths(server: string, profile = 'default', env = process.env): [string, string] {
  if (!PROFILE_RE.test(profile)) throw new Error("Profile must contain only letters, numbers, '-' or '_'");
  const base = configBase(env);
  const filename = `${createHash('sha256').update(server).digest('hex').slice(0, 16)}-${profile}.json`;
  const current = path.join(base, 'remote-device-hub', filename);
  const legacy = path.join(base, 'remote-device-host', filename);
  return [current, legacy];
}

function existsNoFollow(filename: string) {
  try { fsSync.lstatSync(filename); return true; } catch (error: any) { if (error.code === 'ENOENT') return false; throw error; }
}

export async function localRoots(items?: string[]): Promise<Record<string, string>> {
  const entries: [string, string][] = items?.length ? items.map((item): [string, string] => {
    const index = item.indexOf('=');
    if (index <= 0) throw new Error('每个 --root 都需要 NAME=PATH');
    return [item.slice(0, index), item.slice(index + 1)];
  }) : [['work', process.cwd()]];
  const result: Record<string, string> = {};
  for (const [name, value] of entries) {
    if (!PROFILE_RE.test(name)) throw new Error('文件目录名称无效');
    const resolved = await fs.realpath(path.resolve(value));
    const stat = await fs.stat(resolved);
    if (!stat.isDirectory()) throw new Error('每个文件目录都必须是已存在的目录');
    result[name] = resolved;
  }
  return result;
}

export async function readConfig(filename: string) {
  assertPrivatePath(filename, false);
  const handle = await fs.open(filename, fsSync.constants.O_RDONLY | (fsSync.constants.O_NOFOLLOW || 0));
  try {
    const stat = await handle.stat();
    if (!stat.isFile())
      throw new Error('设备配置必须由当前用户持有，且权限为 0600');
    return AgentConfig.validate(JSON.parse(await handle.readFile('utf8')));
  } finally { await handle.close(); }
}

export async function writeConfig(filename: string, config: DeviceConfig) {
  if (process.platform === 'win32') ensurePrivatePath(path.dirname(filename), true);
  const data = `${JSON.stringify(AgentConfig.validate(config), null, 2)}\n`;
  const handle = await fs.open(filename, 'wx', 0o600);
  try { secureNewPath(filename, false); await handle.writeFile(data, 'utf8'); }
  finally { await handle.close(); }
}
