import fs from 'node:fs';
import path from 'node:path';
import { randomBytes } from 'node:crypto';

const O_DIRECTORY = fs.constants.O_DIRECTORY || 0;
const O_NOFOLLOW = fs.constants.O_NOFOLLOW || 0;
const O_NONBLOCK = fs.constants.O_NONBLOCK || 0;

const error = (message, code) => Object.assign(new Error(message), { code });
const contained = (root, value) => {
  const relative = path.relative(root, value);
  return relative === '' || (relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
};
const sameStat = (left, right) => left.dev === right.dev && left.ino === right.ino;

function checkedName(name) {
  if (typeof name !== 'string' || !name || name === '.' || name === '..' || name.includes('\0') || name.includes('/') || name.includes(path.sep)) {
    throw error('Invalid directory entry name', 'EINVAL');
  }
  return name;
}

function typeOf(stat) {
  return stat.isSymbolicLink() ? 'symlink' : stat.isDirectory() ? 'directory' : stat.isFile() ? 'file' : 'other';
}

function verify(handle) {
  if (!handle || typeof handle !== 'object' || !Number.isInteger(handle.fd)) throw error('Invalid directory handle', 'EBADF');
  if (!contained(handle.root, handle.path)) throw error('Directory escaped its configured root', 'EACCES');
  try {
    if (fs.realpathSync(handle.path) !== handle.path) throw error('Directory path changed', 'ESTALE');
  } catch (e) {
    if (e.code === 'ESTALE') throw e;
    throw error('Directory path changed', 'ESTALE');
  }
  let current;
  try { current = fs.fstatSync(handle.fd); } catch { throw error('Directory handle is closed', 'EBADF'); }
  let named;
  try { named = fs.lstatSync(handle.path); } catch { throw error('Directory path changed', 'ESTALE'); }
  if (!current.isDirectory() || !sameStat(current, named) || !sameStat(current, handle)) throw error('Directory path changed', 'ESTALE');
  return current;
}

function openChild(parent, name) {
  checkedName(name);
  verify(parent);
  const childPath = path.join(parent.path, name);
  if (!contained(parent.root, childPath)) throw error('Directory escaped its configured root', 'EACCES');
  const named = fs.lstatSync(childPath);
  if (!named.isDirectory() || named.isSymbolicLink()) throw error('Not a directory', 'ENOTDIR');
  const fd = fs.openSync(childPath, fs.constants.O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  try {
    const opened = fs.fstatSync(fd);
    if (!opened.isDirectory() || !sameStat(named, opened)) throw error('Directory path changed', 'ESTALE');
    return { fd, path: childPath, root: parent.root, dev: opened.dev, ino: opened.ino };
  } catch (e) { fs.closeSync(fd); throw e; }
}

export function openDirectory(baseAbsolute, relative = '.') {
  const requestedRoot = path.resolve(String(baseAbsolute));
  const root = fs.realpathSync(requestedRoot);
  if (root !== requestedRoot) throw error('Root must be a canonical directory', 'EACCES');
  const rootStat = fs.lstatSync(root);
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) throw error('Root is not a directory', 'ENOTDIR');
  if (typeof relative !== 'string' || path.posix.isAbsolute(relative) || path.isAbsolute(relative) || relative.includes('\0')) {
    throw error('Invalid relative directory path', 'EINVAL');
  }
  const rawParts = relative === '.' || relative === '' ? [] : relative.split('/');
  if (rawParts.includes('..')) throw error('Invalid relative directory path', 'EINVAL');
  const parts = rawParts.filter((part) => part && part !== '.');
  let handle;
  const fd = fs.openSync(root, fs.constants.O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  try {
    const opened = fs.fstatSync(fd);
    if (!sameStat(rootStat, opened)) throw error('Directory path changed', 'ESTALE');
    handle = { fd, path: root, root, dev: opened.dev, ino: opened.ino };
    for (const part of parts) {
      const child = openChild(handle, part);
      fs.closeSync(handle.fd);
      handle = child;
    }
    return handle;
  } catch (e) {
    if (handle) fs.closeSync(handle.fd); else fs.closeSync(fd);
    throw e;
  }
}

export function close(handle) {
  if (handle && Number.isInteger(handle.fd)) {
    try { fs.closeSync(handle.fd); } catch (e) { if (e.code !== 'EBADF') throw e; }
  }
}

export function readFile(parent, name, maxBytes = 1024 * 1024) {
  checkedName(name); verify(parent);
  const target = path.join(parent.path, name);
  const named = fs.lstatSync(target);
  if (named.isSymbolicLink() || !named.isFile()) throw error('Only regular files are supported', 'EINVAL');
  const fd = fs.openSync(target, fs.constants.O_RDONLY | O_NOFOLLOW | O_NONBLOCK);
  try {
    const opened = fs.fstatSync(fd);
    if (!opened.isFile()) throw error('Only regular files are supported', 'EINVAL');
    if (opened.size > maxBytes) throw error('File exceeds 1 MiB', 'EFBIG');
    const data = Buffer.alloc(maxBytes + 1);
    let offset = 0;
    while (offset < data.length) {
      const result = fs.readSync(fd, data, offset, data.length - offset, offset);
      if (!result) break;
      offset += result;
    }
    if (offset > maxBytes) throw error('File exceeds 1 MiB', 'EFBIG');
    verify(parent);
    return data.subarray(0, offset);
  } finally { fs.closeSync(fd); }
}

export function statAt(parent, name) {
  checkedName(name); verify(parent);
  const stat = fs.lstatSync(path.join(parent.path, name));
  verify(parent);
  return { type: typeOf(stat), size_bytes: stat.size, mode: stat.mode };
}

export function listDirectory(parent) {
  verify(parent);
  const entries = [];
  const directory = fs.opendirSync(parent.path);
  try {
    while (true) {
      const entry = directory.readSync();
      if (entry === null) break;
      if (entries.length >= 10000) throw error('Directory exceeds 10000 entries; narrow with a shell command', 'E2BIG');
      const stat = fs.lstatSync(path.join(parent.path, entry.name));
      entries.push({ name: entry.name, type: typeOf(stat), size_bytes: stat.size });
    }
  } finally { directory.closeSync(); }
  verify(parent);
  return entries;
}

export function writeAtomic(parent, name, bytes, mode = 0o600, createOnly = false) {
  checkedName(name); verify(parent);
  const target = path.join(parent.path, name);
  let existing = null;
  try { existing = fs.lstatSync(target); } catch (e) { if (e.code !== 'ENOENT') throw e; }
  if (createOnly && existing) throw error('File already exists', 'EEXIST');
  if (existing?.isSymbolicLink() || (existing && !existing.isFile())) throw error('Only regular files are supported', 'EINVAL');
  const temporary = path.join(parent.path, `.rdh-${randomBytes(12).toString('hex')}`);
  let tempfd;
  try {
    tempfd = fs.openSync(temporary, fs.constants.O_WRONLY | fs.constants.O_CREAT | fs.constants.O_EXCL | O_NOFOLLOW, mode & 0o777);
    fs.fchmodSync(tempfd, mode & 0o777);
    let offset = 0;
    while (offset < bytes.length) offset += fs.writeSync(tempfd, bytes, offset, bytes.length - offset);
    fs.fsyncSync(tempfd);
    fs.closeSync(tempfd); tempfd = undefined;
    verify(parent);
    if (createOnly) fs.linkSync(temporary, target);
    else fs.renameSync(temporary, target);
  } finally {
    if (tempfd !== undefined) fs.closeSync(tempfd);
    try { fs.unlinkSync(temporary); } catch (e) { if (e.code !== 'ENOENT') throw e; }
  }
  verify(parent);
}

export function mkdirAt(parent, name) {
  checkedName(name); verify(parent);
  fs.mkdirSync(path.join(parent.path, name), { mode: 0o700 });
  verify(parent);
}

export function dupDirChild(parent, name) { return openChild(parent, name); }

export default { openDirectory, close, readFile, statAt, listDirectory, writeAtomic, mkdirAt, dupDirChild };
