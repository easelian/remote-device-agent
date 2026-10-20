import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { AuditLog, AUDIT_DEFAULTS } from './audit.mjs';
import { Policy } from './policy.mjs';
import { LIMIT_RANGES, SANDBOX_MODES, defaultHiddenPaths, canonicalPath, probeBwrap } from './sandbox.mjs';

// Hardening lives in a sidecar file next to the device config
// (<name>.policy.json) instead of the shared device config. The device config
// schema is a cross-implementation contract (unknown fields are rejected by
// other implementations and by older Agents), so extending it would break
// rollbacks. A missing sidecar means "behave exactly as before".
const TOP_KEYS = new Set(['version', 'allowed_operations', 'readonly_roots', 'protected_paths', 'sandbox', 'audit']);
const SANDBOX_KEYS = new Set(['mode', 'network', 'hide_home', 'expose', 'limits']);
const AUDIT_KEYS = new Set(['enabled', 'max_bytes', 'max_files', 'required']);
const isObject = (value) => value !== null && typeof value === 'object' && !Array.isArray(value);

export function hardeningPath(configFile) {
  return configFile.endsWith('.json') ? `${configFile.slice(0, -5)}.policy.json` : `${configFile}.policy.json`;
}

function rejectUnknown(value, allowed, label) {
  for (const key of Object.keys(value)) if (!allowed.has(key)) throw new Error(`${label}包含未知字段：${key}`);
}

export function readSidecar(filename) {
  let fd;
  try { fd = fs.openSync(filename, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW); }
  catch (error) {
    if (error.code === 'ENOENT') return null;
    throw new Error('无法安全读取硬化配置（不允许符号链接）');
  }
  try {
    const stat = fs.fstatSync(fd);
    const uid = typeof process.getuid === 'function' ? process.getuid() : stat.uid;
    if (!stat.isFile() || stat.uid !== uid || (stat.mode & 0o077)) throw new Error('硬化配置必须由当前用户持有，且权限为 0600');
    if (stat.size > 64 * 1024) throw new Error('硬化配置过大');
    return JSON.parse(fs.readFileSync(fd, 'utf8'));
  } finally { fs.closeSync(fd); }
}

export function validateHardening(value, rootNames = []) {
  if (!isObject(value)) throw new Error('硬化配置格式无效');
  rejectUnknown(value, TOP_KEYS, '硬化配置');
  if (value.version !== 1) throw new Error('硬化配置 version 必须为 1');
  for (const root of value.readonly_roots ?? []) {
    if (!rootNames.includes(root)) throw new Error(`readonly_roots 引用了未配置的目录：${root}`);
  }
  const hasPolicy = ['allowed_operations', 'readonly_roots', 'protected_paths'].some((key) => value[key] !== undefined);
  const policy = hasPolicy ? new Policy({ allowed_operations: value.allowed_operations ?? null,
    readonly_roots: value.readonly_roots ?? [], protected_paths: value.protected_paths ?? [] }) : null;

  let sandbox = null;
  if (value.sandbox !== undefined) {
    const raw = value.sandbox;
    if (!isObject(raw)) throw new Error('sandbox 配置格式无效');
    rejectUnknown(raw, SANDBOX_KEYS, 'sandbox ');
    if (!SANDBOX_MODES.includes(raw.mode)) throw new Error('sandbox.mode 必须是 "none" 或 "bwrap"');
    const limits = raw.limits ?? {};
    if (!isObject(limits)) throw new Error('sandbox.limits 格式无效');
    for (const [key, limit] of Object.entries(limits)) {
      const range = LIMIT_RANGES[key];
      if (!range || !Number.isSafeInteger(limit) || limit < range[0] || limit > range[1]) throw new Error(`sandbox.limits.${key} 无效`);
    }
    if (raw.mode === 'bwrap') {
      if (typeof raw.network !== 'boolean') throw new Error('sandbox.network 必须显式设置为 true 或 false');
      if (raw.hide_home !== undefined && typeof raw.hide_home !== 'boolean') throw new Error('sandbox.hide_home 必须是布尔值');
      const expose = raw.expose ?? [];
      if (!Array.isArray(expose) || expose.length > 32 || expose.some((item) => typeof item !== 'string' || !path.isAbsolute(item) || item.includes('\0'))) {
        throw new Error('sandbox.expose 必须是最多 32 个绝对路径');
      }
      sandbox = { mode: 'bwrap', network: raw.network, hideHome: raw.hide_home ?? true, expose, limits };
    } else {
      if (['network', 'hide_home', 'expose'].some((key) => raw[key] !== undefined)) {
        throw new Error('sandbox.network、hide_home、expose 仅在 mode 为 "bwrap" 时有效');
      }
      sandbox = { mode: 'none', limits };
    }
  }

  const audit = { enabled: false, maxBytes: AUDIT_DEFAULTS.maxBytes, maxFiles: AUDIT_DEFAULTS.maxFiles, required: false };
  if (value.audit !== undefined) {
    if (!isObject(value.audit)) throw new Error('audit 配置格式无效');
    rejectUnknown(value.audit, AUDIT_KEYS, 'audit ');
    const { enabled = false, max_bytes: maxBytes = audit.maxBytes, max_files: maxFiles = audit.maxFiles, required = false } = value.audit;
    if (typeof enabled !== 'boolean' || typeof required !== 'boolean' || !Number.isSafeInteger(maxBytes) || maxBytes < 4096 ||
        !Number.isSafeInteger(maxFiles) || maxFiles < 1 || maxFiles > 20) throw new Error('audit 配置参数无效');
    if (required && !enabled) throw new Error('audit.required 需要 audit.enabled 为 true');
    Object.assign(audit, { enabled, maxBytes, maxFiles, required });
  }
  return { policy, sandbox, readonlyRoots: value.readonly_roots ?? [], audit };
}

export function auditDirectory(env = process.env) {
  return path.join(env.XDG_STATE_HOME || path.join(os.homedir(), '.local', 'state'), 'remote-device-hub', 'audit');
}

// Builds the runtime options handed to Agent. Anything that is configured but
// cannot be honoured (unreadable sidecar, missing bwrap, required audit log
// that cannot be opened) stops the Agent instead of degrading silently.
export function buildHardening(config, configFile, { env = process.env, log = console } = {}) {
  const raw = configFile ? readSidecar(hardeningPath(configFile)) : null;
  const loaded = validateHardening(raw ?? { version: 1 }, Object.keys(config.roots));
  let sandbox = null;
  if (loaded.sandbox) {
    sandbox = { ...loaded.sandbox, readonlyRoots: loaded.readonlyRoots };
    if (sandbox.mode === 'bwrap') {
      probeBwrap();
      sandbox.home = os.homedir();
      sandbox.hiddenPaths = defaultHiddenPaths(env, sandbox.home);
      sandbox.hiddenFiles = configFile ? [configFile, hardeningPath(configFile)].map(canonicalPath) : [];
      sandbox.expose = sandbox.expose.filter((item) => fs.existsSync(item));
    }
  }
  let audit = null;
  if (loaded.audit.enabled) {
    try {
      audit = new AuditLog({ directory: auditDirectory(env), name: config.device_id,
        maxBytes: loaded.audit.maxBytes, maxFiles: loaded.audit.maxFiles });
    } catch (error) {
      if (loaded.audit.required) throw new Error(`审计日志不可用，且配置要求必须启用：${error.message}`);
      log.error(`Audit log disabled: ${error.message}`);
    }
  }
  return { policy: loaded.policy, sandbox, audit };
}
