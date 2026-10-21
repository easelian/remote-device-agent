import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { assertPrivatePath, stateBase } from './platform.js';
import type { DeviceConfig, SandboxSettings } from './types.js';
import { AuditLog, AUDIT_DEFAULTS } from './audit.js';
import { Policy, KNOWN_OPERATIONS } from './policy.js';
import {
  errorCode,
  errorMessage,
  isRecord,
  isSafeInteger,
  isStringArray,
  type UnknownRecord,
} from './boundary.js';
import { LIMIT_RANGES, defaultHiddenPaths, canonicalPath, probeBwrap } from './sandbox.js';

// Hardening lives in a sidecar file next to the device config
// (<name>.policy.json) instead of the shared device config. The device config
// schema is a cross-implementation contract (unknown fields are rejected by
// other implementations and by older Agents), so extending it would break
// rollbacks. A missing sidecar means "behave exactly as before".
const TOP_KEYS = new Set([
  'version',
  'allowed_operations',
  'readonly_roots',
  'protected_paths',
  'sandbox',
  'audit',
]);
const SANDBOX_KEYS = new Set(['mode', 'network', 'hide_home', 'expose', 'limits']);
const AUDIT_KEYS = new Set(['enabled', 'max_bytes', 'max_files', 'required']);
interface AuditSettings {
  enabled: boolean;
  maxBytes: number;
  maxFiles: number;
  required: boolean;
}
export interface ValidatedHardening {
  policy: Policy | null;
  sandbox: SandboxSettings | null;
  readonlyRoots: string[];
  audit: AuditSettings;
}

function policyStrings(value: unknown, label: string, limit: number): string[] {
  if (!isStringArray(value) || value.length > limit) {
    throw new Error(`${label} must be an array of at most ${limit} strings`);
  }
  return value;
}

export function hardeningPath(configFile: string) {
  return configFile.endsWith('.json')
    ? `${configFile.slice(0, -5)}.policy.json`
    : `${configFile}.policy.json`;
}

function rejectUnknown(value: UnknownRecord, allowed: ReadonlySet<string>, label: string) {
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) {
      throw new Error(`${label}包含未知字段：${key}`);
    }
  }
}

export function readSidecar(filename: string): unknown {
  let fd;
  try {
    assertPrivatePath(filename, false);
    fd = fs.openSync(filename, fs.constants.O_RDONLY | (fs.constants.O_NOFOLLOW || 0));
  } catch (error) {
    if (errorCode(error) === 'ENOENT') {
      return null;
    }
    throw new Error('无法安全读取硬化配置（要求 0600；不允许符号链接）');
  }
  try {
    const stat = fs.fstatSync(fd);
    const uid = typeof process.getuid === 'function' ? process.getuid() : stat.uid;
    if (!stat.isFile() || stat.uid !== uid || (process.platform !== 'win32' && stat.mode & 0o077)) {
      throw new Error('硬化配置必须由当前用户持有，且权限为 0600');
    }
    if (stat.size > 64 * 1024) {
      throw new Error('硬化配置过大');
    }
    return JSON.parse(fs.readFileSync(fd, 'utf8'));
  } finally {
    fs.closeSync(fd);
  }
}

export function validateHardening(
  value: unknown,
  rootNames: readonly string[] = [],
): ValidatedHardening {
  if (!isRecord(value)) {
    throw new Error('硬化配置格式无效');
  }
  rejectUnknown(value, TOP_KEYS, '硬化配置');
  if (value.version !== 1) {
    throw new Error('硬化配置 version 必须为 1');
  }
  const readonlyRoots = policyStrings(value.readonly_roots ?? [], 'readonly_roots', 64);
  for (const root of readonlyRoots) {
    if (!rootNames.includes(root)) {
      throw new Error(`readonly_roots 引用了未配置的目录：${root}`);
    }
  }
  const hasPolicy = ['allowed_operations', 'readonly_roots', 'protected_paths'].some(
    (key) => value[key] !== undefined,
  );
  const allowed =
    value.allowed_operations == null
      ? null
      : policyStrings(value.allowed_operations, 'allowed_operations', KNOWN_OPERATIONS.length);
  const protectedPaths = policyStrings(value.protected_paths ?? [], 'protected_paths', 64);
  const policy = hasPolicy
    ? new Policy({
        allowed_operations: allowed,
        readonly_roots: readonlyRoots,
        protected_paths: protectedPaths,
      })
    : null;

  let sandbox: SandboxSettings | null = null;
  if (value.sandbox !== undefined) {
    const raw = value.sandbox;
    if (!isRecord(raw)) {
      throw new Error('sandbox 配置格式无效');
    }
    rejectUnknown(raw, SANDBOX_KEYS, 'sandbox ');
    if (raw.mode !== 'none' && raw.mode !== 'bwrap') {
      throw new Error('sandbox.mode 必须是 "none" 或 "bwrap"');
    }
    const rawLimits = raw.limits ?? {};
    const limits: Record<string, number> = {};
    if (!isRecord(rawLimits)) {
      throw new Error('sandbox.limits 格式无效');
    }
    for (const [key, limit] of Object.entries(rawLimits)) {
      const range = LIMIT_RANGES[key];
      if (
        !Object.hasOwn(LIMIT_RANGES, key) ||
        !range ||
        typeof limit !== 'number' ||
        !Number.isSafeInteger(limit) ||
        limit < range[0] ||
        limit > range[1]
      ) {
        throw new Error(`sandbox.limits.${key} 无效`);
      }
      limits[key] = limit;
    }
    if (raw.mode === 'bwrap') {
      if (typeof raw.network !== 'boolean') {
        throw new Error('sandbox.network 必须显式设置为 true 或 false');
      }
      if (raw.hide_home !== undefined && typeof raw.hide_home !== 'boolean') {
        throw new Error('sandbox.hide_home 必须是布尔值');
      }
      const expose = raw.expose ?? [];
      if (
        !isStringArray(expose) ||
        expose.length > 32 ||
        expose.some((item) => !path.isAbsolute(item) || item.includes('\0'))
      ) {
        throw new Error('sandbox.expose 必须是最多 32 个绝对路径');
      }
      sandbox = {
        mode: 'bwrap',
        network: raw.network,
        hideHome: raw.hide_home ?? true,
        expose,
        limits,
      };
    } else {
      if (['network', 'hide_home', 'expose'].some((key) => raw[key] !== undefined)) {
        throw new Error('sandbox.network、hide_home、expose 仅在 mode 为 "bwrap" 时有效');
      }
      sandbox = { mode: 'none', limits };
    }
  }

  const audit: AuditSettings = {
    enabled: false,
    maxBytes: AUDIT_DEFAULTS.maxBytes,
    maxFiles: AUDIT_DEFAULTS.maxFiles,
    required: false,
  };
  if (value.audit !== undefined) {
    if (!isRecord(value.audit)) {
      throw new Error('audit 配置格式无效');
    }
    rejectUnknown(value.audit, AUDIT_KEYS, 'audit ');
    const {
      enabled = false,
      max_bytes: maxBytes = audit.maxBytes,
      max_files: maxFiles = audit.maxFiles,
      required = false,
    } = value.audit;
    if (
      typeof enabled !== 'boolean' ||
      typeof required !== 'boolean' ||
      !isSafeInteger(maxBytes) ||
      maxBytes < 4096 ||
      !isSafeInteger(maxFiles) ||
      maxFiles < 1 ||
      maxFiles > 20
    ) {
      throw new Error('audit 配置参数无效');
    }
    if (required && !enabled) {
      throw new Error('audit.required 需要 audit.enabled 为 true');
    }
    Object.assign(audit, { enabled, maxBytes, maxFiles, required });
  }
  return { policy, sandbox, readonlyRoots, audit };
}

export function auditDirectory(env: NodeJS.ProcessEnv = process.env) {
  return path.join(stateBase(env), 'remote-device-hub', 'audit');
}

// Builds the runtime options handed to Agent. Anything that is configured but
// cannot be honoured (unreadable sidecar, missing bwrap, required audit log
// that cannot be opened) stops the Agent instead of degrading silently.
export function buildHardening(
  config: DeviceConfig,
  configFile?: string,
  {
    env = process.env,
    log = console,
  }: { env?: NodeJS.ProcessEnv; log?: Pick<Console, 'error'> } = {},
) {
  const raw = configFile ? readSidecar(hardeningPath(configFile)) : null;
  const loaded = validateHardening(raw ?? { version: 1 }, Object.keys(config.roots));
  let sandbox: SandboxSettings | null = null;
  if (loaded.sandbox) {
    sandbox = { ...loaded.sandbox, readonlyRoots: loaded.readonlyRoots };
    if (sandbox.mode === 'bwrap' && process.platform !== 'linux') {
      throw new Error('bubblewrap sandbox is supported only on Linux');
    }
    if (process.platform === 'win32' && Object.keys(sandbox.limits).length) {
      throw new Error('POSIX resource limits are unavailable on Windows');
    }
    if (sandbox.mode === 'bwrap') {
      probeBwrap();
      sandbox.home = os.homedir();
      sandbox.hiddenPaths = defaultHiddenPaths(env, sandbox.home);
      sandbox.hiddenFiles = configFile
        ? [configFile, hardeningPath(configFile)].map(canonicalPath)
        : [];
      sandbox.expose = (sandbox.expose ?? []).filter((item: string) => fs.existsSync(item));
    }
  }
  let audit: AuditLog | null = null;
  if (loaded.audit.enabled) {
    try {
      audit = new AuditLog({
        directory: auditDirectory(env),
        name: config.device_id,
        maxBytes: loaded.audit.maxBytes,
        maxFiles: loaded.audit.maxFiles,
      });
    } catch (error) {
      if (loaded.audit.required) {
        throw new Error(`审计日志不可用，且配置要求必须启用：${errorMessage(error)}`);
      }
      log.error(`Audit log disabled: ${errorMessage(error)}`);
    }
  }
  return { policy: loaded.policy, sandbox, audit };
}
