import fs, { type Stats } from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { validateRelativePath } from './platform.mjs';
import type { FileHandle } from 'node:fs/promises';

interface DirectoryFile {
  fd: number;
  stat(): Promise<Stats>;
  sync(): Promise<void>;
  close(): Promise<void>;
}

export interface DirectoryHandle {
  file: DirectoryFile;
  fd: number;
  path: string;
  root: string;
  dev: number;
  ino: number;
}

interface InodeIdentity {
  dev: number;
  ino: number;
}

export interface DirectoryEntry {
  name: string;
  type: 'symlink' | 'directory' | 'file' | 'other';
  size_bytes: number;
}

interface ListOptions {
  maxEntries?: number;
  signal?: AbortSignal;
  allowTruncated?: boolean;
}

// Windows cannot open directory FileHandles through Node's fs.open API.
// Retain a canonical-path/inode snapshot and revalidate around every operation.
// This is a file-tool boundary, not an OS sandbox against local rename races.
async function directoryFile(filename: string): Promise<DirectoryFile> {
  if (process.platform !== 'win32') return fsp.open(filename, C.O_RDONLY | O_DIRECTORY | O_NOFOLLOW);
  return { fd: -1, stat: () => fsp.lstat(filename), sync: async () => {}, close: async () => {} };
}

const C = fs.constants;
const O_DIRECTORY = C.O_DIRECTORY || 0;
const O_NOFOLLOW = C.O_NOFOLLOW || 0;
const O_NONBLOCK = C.O_NONBLOCK || 0;
const error = (message: string, code: string) => Object.assign(new Error(message), { code });
const same = (a: InodeIdentity, b: InodeIdentity) => a.dev === b.dev && a.ino === b.ino;
const contained = (root: string, value: string) => {
  const relative = path.relative(root, value);
  return relative === '' || (relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative));
};
const checkedName = (name: string) => {
  if (typeof name !== 'string' || !name || name === '.' || name === '..' ||
      name.includes('\0') || name.includes('/') || name.includes(path.sep)) {
    throw error('Invalid directory entry name', 'EINVAL');
  }
  if (process.platform === 'win32') validateRelativePath(name);
};
const kind = (stat: Stats): DirectoryEntry['type'] => stat.isSymbolicLink() ? 'symlink' : stat.isDirectory() ? 'directory' : stat.isFile() ? 'file' : 'other';
const abortCheck = (signal?: AbortSignal) => {
  if (signal?.aborted) throw signal.reason || error('Operation cancelled', 'ECANCELED');
};

async function syncDirectory(handle: DirectoryHandle) {
  try {
    await handle.file.sync();
  } catch (e) {
    if (!['EINVAL', 'ENOTSUP'].includes((e as NodeJS.ErrnoException).code ?? '')) throw e;
  }
}

async function verify(h: DirectoryHandle): Promise<Stats> {
  if (!h || !h.file || !Number.isInteger(h.fd)) {
    throw error('Invalid directory handle', 'EBADF');
  }
  if (!contained(h.root, h.path)) throw error('Directory escaped its configured root', 'EACCES');
  try {
    if (await fsp.realpath(h.path) !== h.path) throw error('Directory path changed', 'ESTALE');
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code === 'ESTALE') throw e;
    throw error('Directory path changed', 'ESTALE');
  }
  let current: Stats, named: Stats;
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

async function openChild(parent: DirectoryHandle, name: string): Promise<DirectoryHandle> {
  checkedName(name);
  await verify(parent);
  const childPath = path.join(parent.path, name);
  if (!contained(parent.root, childPath)) throw error('Directory escaped its configured root', 'EACCES');
  const named = await fsp.lstat(childPath);
  if (!named.isDirectory() || named.isSymbolicLink()) throw error('Not a directory', 'ENOTDIR');
  const file = await directoryFile(childPath);
  try {
    const opened = await file.stat();
    if (!opened.isDirectory() || !same(named, opened)) throw error('Directory path changed', 'ESTALE');
    return { file, fd: file.fd, path: childPath, root: parent.root, dev: opened.dev, ino: opened.ino };
  } catch (e) {
    await file.close().catch(() => {});
    throw e;
  }
}

export async function openDirectory(baseAbsolute: string, relative = '.'): Promise<DirectoryHandle> {
  const requested = path.resolve(String(baseAbsolute));
  const root = await fsp.realpath(requested);
  if (root !== requested) throw error('Root must be a canonical directory', 'EACCES');
  const rootStat = await fsp.lstat(root);
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) throw error('Root is not a directory', 'ENOTDIR');
  if (typeof relative !== 'string' || path.posix.isAbsolute(relative) || path.isAbsolute(relative) || relative.includes('\0')) {
    throw error('Invalid relative directory path', 'EINVAL');
  }
  const parts = validateRelativePath(relative);
  const file = await directoryFile(root);
  let handle: DirectoryHandle | undefined;
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

export async function close(handle: DirectoryHandle | undefined) {
  if (handle?.file) await handle.file.close().catch((e: unknown) => { if ((e as NodeJS.ErrnoException).code !== 'EBADF') throw e; });
}

export async function readFile(parent: DirectoryHandle, name: string, maxBytes = 1024 * 1024) {
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

export async function statAt(parent: DirectoryHandle, name: string) {
  checkedName(name);
  await verify(parent);
  const stat = await fsp.lstat(path.join(parent.path, name));
  await verify(parent);
  return { type: kind(stat), size_bytes: stat.size, mode: stat.mode, mtime_ms: stat.mtimeMs, dev: stat.dev, ino: stat.ino };
}

export async function statDirectory(handle: DirectoryHandle) {
  const stat = await verify(handle);
  return { type: 'directory', size_bytes: stat.size, mode: stat.mode, mtime_ms: stat.mtimeMs, dev: stat.dev, ino: stat.ino };
}

export async function readFileChunk(parent: DirectoryHandle, name: string, offset: number, length: number, maxBytes = 64 * 1024 * 1024) {
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

export async function listDirectory(parent: DirectoryHandle, options?: ListOptions & { allowTruncated?: false }): Promise<DirectoryEntry[]>;
export async function listDirectory(parent: DirectoryHandle, options: ListOptions & { allowTruncated: true }): Promise<{ entries: DirectoryEntry[]; truncated: boolean; reason?: string }>;
export async function listDirectory(parent: DirectoryHandle, options: ListOptions): Promise<DirectoryEntry[] | { entries: DirectoryEntry[]; truncated: boolean; reason?: string }>;
export async function listDirectory(parent: DirectoryHandle, options: ListOptions = {}) {
  const { maxEntries = 10000, signal, allowTruncated = false } = options;
  if (!Number.isSafeInteger(maxEntries) || maxEntries < 0 || typeof allowTruncated !== 'boolean') {
    throw error('Invalid directory enumeration limits', 'EINVAL');
  }
  await verify(parent);
  const directory = await fsp.opendir(parent.path);
  const entries: DirectoryEntry[] = [];
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

export async function writeAtomic(parent: DirectoryHandle, name: string, bytes: Uint8Array, mode = 0o600, createOnly = false) {
  checkedName(name);
  await verify(parent);
  const target = path.join(parent.path, name);
  let existing: Stats | null = null;
  try { existing = await fsp.lstat(target); }
  catch (e) { if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e; }
  if (createOnly && existing) throw error('File already exists', 'EEXIST');
  if (existing?.isSymbolicLink() || (existing && !existing.isFile())) throw error('Only regular files are supported', 'EINVAL');
  const temporary = path.join(parent.path, `.rdh-${randomBytes(12).toString('hex')}`);
  let file: FileHandle | null | undefined;
  try {
    file = await fsp.open(temporary, C.O_WRONLY | C.O_CREAT | C.O_EXCL | O_NOFOLLOW, mode & 0o777);
    if (process.platform !== 'win32') await file.chmod(mode & 0o777);
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
    await fsp.unlink(temporary).catch((e: unknown) => { if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e; });
  }
  await verify(parent);
}

export async function moveAtomic(sourceParent: DirectoryHandle, sourceName: string, destinationParent: DirectoryHandle, destinationName: string) {
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
  catch (e) { if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e; }
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
    if ((e as NodeJS.ErrnoException).code === 'EXDEV') throw error('Moving files across filesystems is disabled', 'EXDEV');
    throw e;
  } finally {
    await file.close();
  }
}

export async function mkdirAt(parent: DirectoryHandle, name: string) {
  checkedName(name);
  await verify(parent);
  await fsp.mkdir(path.join(parent.path, name), { mode: 0o700 });
  await verify(parent);
}

export async function dupDirChild(parent: DirectoryHandle, name: string) {
  return openChild(parent, name);
}

export default {
  openDirectory, close, readFile, readFileChunk, statAt, statDirectory,
  listDirectory, writeAtomic, moveAtomic, mkdirAt, dupDirChild,
};
