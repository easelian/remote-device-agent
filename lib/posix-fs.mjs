import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { randomBytes } from 'node:crypto';

const C = fs.constants;
const O_DIRECTORY = C.O_DIRECTORY || 0;
const O_NOFOLLOW = C.O_NOFOLLOW || 0;
const O_NONBLOCK = C.O_NONBLOCK || 0;
const error = (message, code) => Object.assign(new Error(message), { code });
const same = (a, b) => a.dev === b.dev && a.ino === b.ino;
const contained = (root, value) => {
  const relative = path.relative(root, value);
  return relative === '' || (relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
};
const checkedName = (name) => {
  if (typeof name !== 'string' || !name || name === '.' || name === '..' ||
      name.includes('\0') || name.includes('/') || name.includes(path.sep)) {
    throw error('Invalid directory entry name', 'EINVAL');
  }
};
const kind = (stat) => stat.isSymbolicLink() ? 'symlink' : stat.isDirectory() ? 'directory' : stat.isFile() ? 'file' : 'other';
const abortCheck = (signal) => {
  if (signal?.aborted) throw signal.reason || error('Operation cancelled', 'ECANCELED');
};

async function syncDirectory(handle) {
  try {
    await handle.file.sync();
  } catch (e) {
    if (!['EINVAL', 'ENOTSUP'].includes(e.code)) throw e;
  }
}

async function verify(h) {
  if (!h || !h.file || !Number.isInteger(h.fd)) {
    throw error('Invalid directory handle', 'EBADF');
  }
  if (!contained(h.root, h.path)) throw error('Directory escaped its configured root', 'EACCES');
  try {
    if (await fsp.realpath(h.path) !== h.path) throw error('Directory path changed', 'ESTALE');
  } catch (e) {
    if (e.code === 'ESTALE') throw e;
    throw error('Directory path changed', 'ESTALE');
  }
  let current, named;
  try {
    [current, named] = await Promise.all([h.file.stat(), fsp.lstat(h.path)]);
  } catch {
    throw error('Directory path changed', 'ESTALE');
  }
  if (!current.isDirectory() || !same(current, named) || !same(current, h)) {
    throw error('Directory path changed', 'ESTALE');
  }
  return current;
}

async function openChild(parent, name) {
  checkedName(name);
  await verify(parent);
  const childPath = path.join(parent.path, name);
  if (!contained(parent.root, childPath)) throw error('Directory escaped its configured root', 'EACCES');
  const named = await fsp.lstat(childPath);
  if (!named.isDirectory() || named.isSymbolicLink()) throw error('Not a directory', 'ENOTDIR');
  const file = await fsp.open(childPath, C.O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  try {
    const opened = await file.stat();
    if (!opened.isDirectory() || !same(named, opened)) throw error('Directory path changed', 'ESTALE');
    return { file, fd: file.fd, path: childPath, root: parent.root, dev: opened.dev, ino: opened.ino };
  } catch (e) {
    await file.close().catch(() => {});
    throw e;
  }
}

export async function openDirectory(baseAbsolute, relative = '.') {
  const requested = path.resolve(String(baseAbsolute));
  const root = await fsp.realpath(requested);
  if (root !== requested) throw error('Root must be a canonical directory', 'EACCES');
  const rootStat = await fsp.lstat(root);
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) throw error('Root is not a directory', 'ENOTDIR');
  if (typeof relative !== 'string' || path.posix.isAbsolute(relative) || path.isAbsolute(relative) || relative.includes('\0')) {
    throw error('Invalid relative directory path', 'EINVAL');
  }
  const raw = relative === '.' || relative === '' ? [] : relative.split('/');
  if (raw.includes('..')) throw error('Invalid relative directory path', 'EINVAL');
  const parts = raw.filter((part) => part && part !== '.');
  const file = await fsp.open(root, C.O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  let handle;
  try {
    const opened = await file.stat();
    if (!same(rootStat, opened)) throw error('Directory path changed', 'ESTALE');
    handle = { file, fd: file.fd, path: root, root, dev: opened.dev, ino: opened.ino };
    for (const part of parts) {
      const child = await openChild(handle, part);
      await handle.file.close();
      handle = child;
    }
    return handle;
  } catch (e) {
    if (handle?.file) await handle.file.close().catch(() => {});
    else await file.close().catch(() => {});
    throw e;
  }
}

export async function close(handle) {
  if (handle?.file) await handle.file.close().catch((e) => { if (e.code !== 'EBADF') throw e; });
}

export async function readFile(parent, name, maxBytes = 1024 * 1024) {
  checkedName(name);
  await verify(parent);
  const target = path.join(parent.path, name);
  const named = await fsp.lstat(target);
  if (named.isSymbolicLink() || !named.isFile()) throw error('Only regular files are supported', 'EINVAL');
  const file = await fsp.open(target, C.O_RDONLY | O_NOFOLLOW | O_NONBLOCK);
  try {
    const opened = await file.stat();
    if (!opened.isFile() || !same(named, opened)) throw error('File changed while opening', 'ESTALE');
    if (opened.size > maxBytes) throw error('File exceeds 1 MiB', 'EFBIG');
    const data = Buffer.alloc(maxBytes + 1);
    let offset = 0;
    while (offset < data.length) {
      const result = await file.read(data, offset, data.length - offset, offset);
      if (!result.bytesRead) break;
      offset += result.bytesRead;
    }
    if (offset > maxBytes) throw error('File exceeds 1 MiB', 'EFBIG');
    await verify(parent);
    return data.subarray(0, offset);
  } finally {
    await file.close();
  }
}

export async function statAt(parent, name) {
  checkedName(name);
  await verify(parent);
  const stat = await fsp.lstat(path.join(parent.path, name));
  await verify(parent);
  return { type: kind(stat), size_bytes: stat.size, mode: stat.mode, mtime_ms: stat.mtimeMs, dev: stat.dev, ino: stat.ino };
}

export async function statDirectory(handle) {
  const stat = await verify(handle);
  return { type: 'directory', size_bytes: stat.size, mode: stat.mode, mtime_ms: stat.mtimeMs, dev: stat.dev, ino: stat.ino };
}

export async function readFileChunk(parent, name, offset, length, maxBytes = 64 * 1024 * 1024) {
  checkedName(name);
  await verify(parent);
  if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(length) || length < 1 || length > 1024 * 1024) {
    throw error('Invalid byte offset or length', 'EINVAL');
  }
  const target = path.join(parent.path, name);
  const named = await fsp.lstat(target);
  if (named.isSymbolicLink() || !named.isFile()) throw error('Only regular files are supported', 'EINVAL');
  const file = await fsp.open(target, C.O_RDONLY | O_NOFOLLOW | O_NONBLOCK);
  try {
    const opened = await file.stat();
    if (!opened.isFile() || !same(named, opened)) throw error('File changed while opening', 'ESTALE');
    if (opened.size > maxBytes) throw error('File exceeds 64 MiB', 'EFBIG');
    const available = Math.max(0, Math.min(length, opened.size - offset));
    const tailLength = Math.min(3, Math.max(0, opened.size - offset - available));
    const data = Buffer.alloc(available + tailLength);
    let read = 0;
    while (read < data.length) {
      const result = await file.read(data, read, data.length - read, offset + read);
      if (!result.bytesRead) break;
      read += result.bytesRead;
    }
    await verify(parent);
    const actual = Math.min(read, available);
    return {
      bytes: data.subarray(0, actual), tail: data.subarray(available, read),
      size_bytes: opened.size, mtime_ms: opened.mtimeMs, mode: opened.mode,
      dev: opened.dev, ino: opened.ino, next_offset: offset + actual,
      truncated: offset + actual < opened.size,
    };
  } finally {
    await file.close();
  }
}

export async function listDirectory(parent, options = {}) {
  const { maxEntries = 10000, signal, allowTruncated = false } = options;
  if (!Number.isSafeInteger(maxEntries) || maxEntries < 0 || typeof allowTruncated !== 'boolean') {
    throw error('Invalid directory enumeration limits', 'EINVAL');
  }
  await verify(parent);
  const directory = await fsp.opendir(parent.path);
  const entries = [];
  let truncated = false;
  try {
    while (true) {
      abortCheck(signal);
      const entry = await directory.read();
      if (!entry) break;
      if (entries.length >= maxEntries) {
        truncated = true;
        if (!allowTruncated) throw error(`Directory exceeds ${maxEntries} entries; narrow with a shell command`, 'E2BIG');
        break;
      }
      const stat = await fsp.lstat(path.join(parent.path, entry.name));
      entries.push({ name: entry.name, type: kind(stat), size_bytes: stat.size });
    }
  } finally {
    await directory.close();
  }
  await verify(parent);
  if (allowTruncated) return { entries, truncated, ...(truncated ? { reason: 'max_visited' } : {}) };
  return entries;
}

export async function writeAtomic(parent, name, bytes, mode = 0o600, createOnly = false) {
  checkedName(name);
  await verify(parent);
  const target = path.join(parent.path, name);
  let existing = null;
  try { existing = await fsp.lstat(target); }
  catch (e) { if (e.code !== 'ENOENT') throw e; }
  if (createOnly && existing) throw error('File already exists', 'EEXIST');
  if (existing?.isSymbolicLink() || (existing && !existing.isFile())) throw error('Only regular files are supported', 'EINVAL');
  const temporary = path.join(parent.path, `.rdh-${randomBytes(12).toString('hex')}`);
  let file;
  try {
    file = await fsp.open(temporary, C.O_WRONLY | C.O_CREAT | C.O_EXCL | O_NOFOLLOW, mode & 0o777);
    await file.chmod(mode & 0o777);
    let offset = 0;
    while (offset < bytes.length) {
      const result = await file.write(bytes, offset, bytes.length - offset, offset);
      if (!result.bytesWritten) throw error('File write made no progress', 'EIO');
      offset += result.bytesWritten;
    }
    await file.sync();
    await file.close();
    file = null;
    await verify(parent);
    if (createOnly) await fsp.link(temporary, target);
    else await fsp.rename(temporary, target);
    await syncDirectory(parent);
  } finally {
    if (file) await file.close().catch(() => {});
    await fsp.unlink(temporary).catch((e) => { if (e.code !== 'ENOENT') throw e; });
  }
  await verify(parent);
}

export async function moveAtomic(sourceParent, sourceName, destinationParent, destinationName) {
  checkedName(sourceName);
  checkedName(destinationName);
  await verify(sourceParent);
  await verify(destinationParent);
  if (sourceParent.root !== destinationParent.root) throw error('Moving files across roots are disabled', 'EXDEV');
  const source = path.join(sourceParent.path, sourceName);
  const destination = path.join(destinationParent.path, destinationName);
  const sourceStat = await fsp.lstat(source);
  if (sourceStat.isSymbolicLink() || !sourceStat.isFile()) throw error('Only regular files can be moved', 'EINVAL');
  try { await fsp.lstat(destination); throw error('Destination already exists', 'EEXIST'); }
  catch (e) { if (e.code !== 'ENOENT') throw e; }
  const file = await fsp.open(source, C.O_RDONLY | O_NOFOLLOW);
  try {
    const opened = await file.stat();
    if (!same(sourceStat, opened)) throw error('Source changed while opening', 'ESTALE');
    await fsp.link(source, destination);
    await verify(sourceParent);
    await verify(destinationParent);
    await fsp.unlink(source);
    await syncDirectory(sourceParent);
    if (destinationParent !== sourceParent) await syncDirectory(destinationParent);
    await verify(sourceParent);
    await verify(destinationParent);
  } catch (e) {
    if (e.code === 'EXDEV') throw error('Moving files across filesystems is disabled', 'EXDEV');
    throw e;
  } finally {
    await file.close();
  }
}

export async function mkdirAt(parent, name) {
  checkedName(name);
  await verify(parent);
  await fsp.mkdir(path.join(parent.path, name), { mode: 0o700 });
  await verify(parent);
}

export async function dupDirChild(parent, name) {
  return openChild(parent, name);
}

export default {
  openDirectory, close, readFile, readFileChunk, statAt, statDirectory,
  listDirectory, writeAtomic, moveAtomic, mkdirAt, dupDirChild,
};
