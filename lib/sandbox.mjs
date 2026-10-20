import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

export const SANDBOX_MODES = Object.freeze(['none', 'bwrap']);
// Inclusive ranges; file_size_blocks uses the unit of the shell's `ulimit -f`
// (512-byte blocks in POSIX sh and dash).
export const LIMIT_RANGES = Object.freeze({ cpu_seconds: [1, 86400], memory_kb: [65536, 1 << 30],
  open_files: [16, 1 << 20], file_size_blocks: [1, 1 << 30] });
const ULIMIT_FLAGS = Object.freeze({ cpu_seconds: 't', memory_kb: 'v', open_files: 'n', file_size_blocks: 'f' });

// Prefix for `sh -c`. A limit that cannot be applied aborts the command with
// status 126 instead of silently running it unrestricted.
export function limitsPrefix(limits = {}) {
  let prefix = '';
  for (const [key, flag] of Object.entries(ULIMIT_FLAGS)) {
    const value = limits[key];
    if (value === undefined) continue;
    const [low, high] = LIMIT_RANGES[key];
    if (!Number.isSafeInteger(value) || value < low || value > high) throw new TypeError(`Invalid limit ${key}`);
    prefix += `ulimit -${flag} ${value} || exit 126; `;
  }
  return prefix;
}

// Resolve existing ancestors too, including paths whose audit directory will
// only be created later. Symlink aliases must not bypass a sensitive mask.
export function canonicalPath(value) {
  const absolute = path.resolve(value);
  try { return fs.realpathSync(absolute); }
  catch (error) {
    if (error.code !== 'ENOENT') throw error;
    const parent = path.dirname(absolute);
    if (parent === absolute) throw error;
    return path.join(canonicalPath(parent), path.basename(absolute));
  }
}

const parentFirst = (a, b) => a.split(path.sep).length - b.split(path.sep).length || a.localeCompare(b);
const within = (child, parent) => child === parent || child.startsWith(parent === '/' ? '/' : `${parent}/`);

// Bind writable roots first, then read-only trees, then sensitive masks.
// A read-only ancestor wins over writable aliases and descendants.
export function buildBwrapArgs({ roots, writableRoots = new Set(), workdir, network, hideHome = true, home,
  expose = [], hiddenPaths = [], hiddenFiles = [], shell, script }) {
  if (typeof network !== 'boolean') throw new TypeError('sandbox network must be set explicitly');
  const args = ['--die-with-parent', '--unshare-pid', '--unshare-ipc', '--unshare-uts'];
  if (!network) args.push('--unshare-net');
  args.push('--ro-bind', '/', '/', '--dev', '/dev', '--proc', '/proc', '--tmpfs', '/tmp');
  if (hideHome && home) args.push('--tmpfs', canonicalPath(home));
  const writable = [], readonly = [...expose];
  for (const [name, root] of Object.entries(roots)) (writableRoots.has(name) ? writable : readonly).push(root);
  const writablePaths = [...new Set(writable.map(canonicalPath))].sort(parentFirst);
  const readonlyPaths = [...new Set(readonly.map(canonicalPath))].sort(parentFirst);
  const bindTrees = (below = null) => {
    for (const [flag, paths] of [['--bind', writablePaths], ['--ro-bind', readonlyPaths]]) {
      for (const root of paths) {
        if (below && !within(root, below)) continue;
        const effectiveFlag = flag === '--bind' && readonlyPaths.some(parent => within(root, parent)) ? '--ro-bind' : flag;
        args.push(effectiveFlag, root, root);
      }
    }
  };
  bindTrees();
  // An ancestor root (including /) can restore host mounts. Rebuild private
  // trees, re-exposing only explicitly configured descendants, and always
  // restore private procfs/devices after every user-controlled bind.
  const privateTrees = ['/tmp', ...(hideHome && home ? [canonicalPath(home)] : [])].sort(parentFirst);
  for (const root of new Set(privateTrees)) {
    args.push('--tmpfs', root);
    bindTrees(root);
  }
  args.push('--dev', '/dev', '--proc', '/proc');
  const masks = [...new Set(hiddenPaths.map(canonicalPath))].sort(parentFirst);
  const parents = masks.filter((item, index) => !masks.slice(0, index).some(parent => within(item, parent)));
  for (const item of parents) args.push('--tmpfs', item);
  for (const item of new Set(hiddenFiles.map(canonicalPath))) {
    if (!parents.some(parent => within(item, parent))) args.push('--ro-bind', '/dev/null', item);
  }
  args.push('--chdir', workdir, '--', shell, '-c', script);
  return args;
}

// Include not-yet-created state directories so the first audit log is hidden.
export function defaultHiddenPaths(env = process.env, home = os.homedir()) {
  const config = env.XDG_CONFIG_HOME || path.join(home, '.config');
  const state = env.XDG_STATE_HOME || path.join(home, '.local', 'state');
  return [path.join(config, 'remote-device-hub'), path.join(config, 'remote-device-host'),
    path.join(state, 'remote-device-hub')].map(canonicalPath);
}

// Fail closed: a configured sandbox that cannot start must stop the Agent
// instead of quietly running commands without isolation.
export function probeBwrap(binary = 'bwrap') {
  const result = spawnSync(binary, ['--ro-bind', '/', '/', '--dev', '/dev', '--proc', '/proc', '--unshare-pid',
    '--die-with-parent', '--tmpfs', '/tmp', 'true'], { timeout: 5000, stdio: 'ignore' });
  if (result.error || result.status !== 0) {
    throw new Error('bubblewrap (bwrap) 不可用或无法创建用户命名空间；sandbox.mode 为 "bwrap" 时拒绝在无隔离状态下启动');
  }
}
