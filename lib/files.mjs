import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import filesystemDefault from './posix-fs.mjs';
import { CASEFOLD_EXCEPTIONS } from './casefold.mjs';

const MAX_FILE = 1024 * 1024;
const MAX_ENTRIES = 10000;
const MAX_SEARCH_DEPTH = 30;
const MAX_SEARCH_VISITED = 10000;
const MAX_CHUNK_FILE = 64 * 1024 * 1024;
const MAX_CHUNK_LENGTH = 65536;
const MAX_MULTI_FILES = 16;
const MAX_MULTI_BYTES = 256 * 1024;

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
const asPromise = (value) => Promise.resolve(value);
const casefold = (value) => Array.from(value, (character) => CASEFOLD_EXCEPTIONS[character] ?? character.toLowerCase()).join('');
const compareCodePoints = (left, right) => {
  const a = Array.from(left);
  const b = Array.from(right);
  for (let i = 0; i < Math.min(a.length, b.length); i++) {
    const difference = a[i].codePointAt(0) - b[i].codePointAt(0);
    if (difference) return difference;
  }
  return a.length - b.length;
};

function pathParts(value) {
  if (typeof value !== 'string' || value.length > 4096 || value.includes('\0')) {
    throw new Error("Path must be relative to a root, with no '..' or NUL");
  }
  const parsed = path.posix.parse(value);
  const parts = value === '.' || value === '' ? [] : value.split('/').filter((part) => part && part !== '.');
  if (parsed.root || parts.includes('..')) {
    throw new Error("Path must be relative to a root, with no '..' or NUL");
  }
  return parts;
}

function isParams(value) {
  return value !== null && typeof value === 'object' && !Buffer.isBuffer(value);
}

export class Files {
  constructor(roots, allowWrite, filesystem = filesystemDefault) {
    if (!roots || typeof roots !== 'object') throw new TypeError('roots must be an object');
    this.roots = {};
    for (const [name, root] of Object.entries(roots)) {
      const canonical = fs.realpathSync(String(root));
      if (!fs.statSync(canonical).isDirectory()) throw new Error('Every root must be an existing directory');
      this.roots[name] = canonical;
    }
    this.allowWrite = Boolean(allowWrite);
    this.filesystem = filesystem;
    this.lock = Promise.resolve();
  }

  static async create(roots, allowWrite, filesystem = filesystemDefault) {
    const canonical = {};
    for (const [name, root] of Object.entries(roots || {})) canonical[name] = await fs.promises.realpath(String(root));
    return new Files(canonical, allowWrite, filesystem);
  }

  parts(root, value) {
    if (!Object.hasOwn(this.roots, root)) throw new Error('Unknown root; use device_info to discover configured roots');
    return [this.roots[root], pathParts(value)];
  }

  async directory(root, value = '.') {
    const [base, parts] = this.parts(root, value);
    return asPromise(this.filesystem.openDirectory(base, parts.length ? parts.join('/') : '.'));
  }

  async close(fd) { return asPromise(this.filesystem.close(fd)); }

  async parent(root, value) {
    const [base, parts] = this.parts(root, value);
    if (!parts.length) throw new Error('A file name is required');
    const parentPath = parts.slice(0, -1).join('/') || '.';
    const fd = await asPromise(this.filesystem.openDirectory(base, parentPath));
    return { fd, name: parts.at(-1) };
  }

  async _read(parentfd, name) {
    const data = Buffer.from(await asPromise(this.filesystem.readFile(parentfd, name, MAX_FILE)));
    if (data.length > MAX_FILE) throw new Error('File exceeds 1 MiB; use a shell command to inspect a bounded portion');
    return data;
  }

  async read(arg, value, offset = 0, length = 32768) {
    const params = isParams(arg) ? arg : { root: arg, path: value, offset, length };
    ({ root: arg, path: value, offset = 0, length = 32768 } = params);
    if (!Number.isInteger(offset) || offset < 0 || !Number.isInteger(length) || length < 1 || length > 65536) {
      throw new Error('Invalid byte offset or length (1..65536)');
    }
    const { fd, name } = await this.parent(arg, value);
    try {
      const data = await this._read(fd, name);
      return { path: value, content: data.subarray(offset, offset + length).toString('utf8'),
        sha256: sha256(data), size_bytes: data.length, next_offset: Math.min(offset + length, data.length),
        truncated: offset + length < data.length };
    } finally { await this.close(fd); }
  }

  async fileInfo(arg, value) {
    const params = isParams(arg) ? arg : { root: arg, path: value };
    ({ root: arg, path: value } = params);
    const parts = this.parts(arg, value)[1];
    const fd = parts.length ? await this.directory(arg, parts.slice(0, -1).join('/') || '.') : await this.directory(arg, '.');
    try {
      const info = parts.length ? await asPromise(this.filesystem.statAt(fd, parts.at(-1))) : await asPromise(this.filesystem.statDirectory(fd));
      if (info.type !== 'file' && info.type !== 'directory') throw new Error('Only regular files and directories are supported');
      return { path: value, type: info.type, size_bytes: info.size_bytes, mode: info.mode & 0o777,
        mtime_ms: info.mtime_ms, read_only: !this.allowWrite,
        fingerprint: `${info.dev}:${info.ino}:${info.size_bytes}:${info.mtime_ms}` };
    } finally { await this.close(fd); }
  }

  async readFileChunk(arg, value, offset = 0, length = 32768) {
    const params = isParams(arg) ? arg : { root: arg, path: value, offset, length };
    let { from_end = false } = params;
    ({ root: arg, path: value, offset = 0, length = 32768 } = params);
    if (typeof from_end !== 'boolean' || !Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(length) || length < 1 || length > MAX_CHUNK_LENGTH) {
      throw new Error('Invalid byte offset or length');
    }
    const { fd, name } = await this.parent(arg, value);
    try {
      const info = await asPromise(this.filesystem.statAt(fd, name));
      if (info.type !== 'file') throw new Error('Only regular files are supported');
      const end = from_end ? Math.max(0, info.size_bytes - offset) : info.size_bytes;
      const start = from_end ? Math.max(0, end - length) : offset;
      const actualLength = from_end ? end - start : length;
      if (actualLength === 0) return { path: value, content: '', content_base64: '', utf8_valid: true, content_encoding: 'utf-8',
        offset: start, next_offset: start, size_bytes: info.size_bytes, truncated: false, read_only: true,
        fingerprint: `${info.dev}:${info.ino}:${info.size_bytes}:${info.mtime_ms}` };
      const chunk = await asPromise(this.filesystem.readFileChunk(fd, name, start, actualLength, MAX_CHUNK_FILE));
      let utf8Valid = true;
      try { new TextDecoder('utf-8', { fatal: true }).decode(chunk.bytes); } catch { utf8Valid = false; }
      return { path: value, content: chunk.bytes.toString('utf8'), content_base64: chunk.bytes.toString('base64'),
        utf8_valid: utf8Valid, content_encoding: utf8Valid ? 'utf-8' : 'utf-8-replacement',
        offset: start, next_offset: chunk.next_offset,
        size_bytes: chunk.size_bytes, truncated: chunk.truncated, read_only: true,
        fingerprint: `${chunk.dev}:${chunk.ino}:${chunk.size_bytes}:${chunk.mtime_ms}` };
    } finally { await this.close(fd); }
  }

  async readMultipleFiles(arg) {
    const params = isParams(arg) ? arg : { paths: arg };
    const canonical = Array.isArray(params.paths);
    const requests = canonical ? params.paths.map((filePath) => ({ root: params.root, path: filePath, offset: params.offset, length: params.length })) : params.files;
    const totalLimit = params.total_limit ?? MAX_MULTI_BYTES;
    if (!Array.isArray(requests) || requests.length < 1 || requests.length > MAX_MULTI_FILES ||
        !Number.isInteger(totalLimit) || totalLimit < 1 || totalLimit > MAX_MULTI_BYTES) throw new Error('Invalid files/total_limit');
    const results = []; let used = 0; let truncated = false;
    const readText = async (request, sourceLength) => {
      const { fd, name } = await this.parent(request.root, request.path);
      try {
        const info = await asPromise(this.filesystem.statAt(fd, name));
        if (info.type !== 'file') throw new Error('Only regular files are supported');
        if (info.size_bytes > MAX_FILE) throw new Error('File exceeds 1 MiB');
        const start = request.offset ?? 0;
        const chunk = await asPromise(this.filesystem.readFileChunk(fd, name, start, sourceLength, MAX_CHUNK_FILE));
        let end = chunk.bytes.length; let text;
        for (let trim = 0; trim <= 3; trim++) {
          try { text = new TextDecoder('utf-8', { fatal: true }).decode(chunk.bytes.subarray(0, end)); break; }
          catch { end -= 1; }
        }
        if (text === undefined || (chunk.bytes.length > 0 && end <= 0)) throw new Error('length cannot fit complete UTF-8 character or invalid UTF-8');
        return { path: request.path, content: text, content_base64: chunk.bytes.subarray(0, end).toString('base64'),
          utf8_valid: true, content_encoding: 'utf-8', offset: start, next_offset: start + end,
          size_bytes: chunk.size_bytes, truncated: start + end < chunk.size_bytes,
          read_only: true, fingerprint: `${chunk.dev}:${chunk.ino}:${chunk.size_bytes}:${chunk.mtime_ms}` };
      } finally { await this.close(fd); }
    };
    for (const request of requests) {
      if (!isParams(request) || typeof request.root !== 'string' || typeof request.path !== 'string') {
        results.push({ path: request?.path, error: 'Invalid file request' }); continue;
      }
      const remaining = totalLimit - used;
      if (remaining < 1) { truncated = true; results.push({ path: request.path, error: 'Total response limit exceeded' }); continue; }
      const length = request.length ?? 32768;
      const offset = request.offset ?? 0;
      if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isInteger(length) || length < 1 || length > 65536 || (canonical && length > 16384)) {
        results.push({ path: request.path, error: 'Invalid byte offset or length' }); continue;
      }
      try {
        const candidate = await readText(request, length);
        let result = candidate;
        let outputBytes = Buffer.byteLength(candidate.content, 'utf8');
        if (outputBytes > remaining) {
          let text = ''; let bytes = 0;
          for (const character of candidate.content) {
            const characterBytes = Buffer.byteLength(character, 'utf8');
            if (bytes + characterBytes > remaining) break;
            text += character; bytes += characterBytes;
          }
          if (!text) { truncated = true; results.push({ path: request.path, error: 'Output budget cannot fit the next UTF-8 character' }); continue; }
          const sourceBytes = Buffer.byteLength(text, 'utf8');
          result = { ...candidate, content: text, content_base64: Buffer.from(text, 'utf8').toString('base64'),
            next_offset: candidate.offset + sourceBytes, truncated: true };
          outputBytes = bytes; truncated = true;
        }
        if (result.truncated) truncated = true;
        used += outputBytes;
        results.push(result);
      } catch (error) { results.push({ path: request.path, error: String(error.message).slice(0, 1000) }); }
    }
    return { files: results, total_bytes: used, truncated };
  }

  async moveFile(arg, value, destinationRoot, destinationPath) {
    const params = isParams(arg) ? arg : { root: arg, path: value, destination_root: destinationRoot, destination_path: destinationPath };
    if (typeof params.source === 'string' || typeof params.destination === 'string') {
      ({ root: arg, source: value, destination: destinationPath } = params);
      destinationRoot = arg;
    } else ({ root: arg, path: value, destination_root: destinationRoot, destination_path: destinationPath } = params);
    if (!this.allowWrite) throw new Error("File writes are disabled in this device's configuration");
    if (arg !== destinationRoot) throw new Error('Moving files across roots is disabled');
    const source = await this.parent(arg, value);
    let destination;
    try {
      destination = await this.parent(destinationRoot, destinationPath);
      const previous = this.lock; let release;
      this.lock = new Promise((resolve) => { release = resolve; });
      await previous;
      try {
        await asPromise(this.filesystem.moveAtomic(source.fd, source.name, destination.fd, destination.name));
      } finally { release(); }
      return { path: value, destination_path: destinationPath, moved: true };
    } finally {
      await this.close(source.fd);
      if (destination) await this.close(destination.fd);
    }
  }

  async list(arg, value = '.', offset = 0, limit = 100) {
    const params = isParams(arg) ? arg : { root: arg, path: value, offset, limit };
    ({ root: arg, path: value = '.', offset = 0, limit = 100 } = params);
    if (!Number.isInteger(offset) || offset < 0 || !Number.isInteger(limit) || limit < 1 || limit > 1000) {
      throw new Error('Invalid offset/limit');
    }
    const fd = await this.directory(arg, value);
    try {
      const entries = await asPromise(this.filesystem.listDirectory(fd));
      if (entries.length > MAX_ENTRIES) throw new Error('Directory exceeds 10000 entries; narrow with a shell command');
      entries.sort((a, b) => compareCodePoints(a.name, b.name));
      return { entries: entries.slice(offset, offset + limit), total: entries.length,
        next_offset: offset + limit < entries.length ? offset + limit : null };
    } finally { await this.close(fd); }
  }

  async write(arg, value, content, expected_sha256 = '') {
    const params = isParams(arg) ? arg : { root: arg, path: value, content, expected_sha256 };
    ({ root: arg, path: value, content, expected_sha256 = '' } = params);
    if (!this.allowWrite) throw new Error("File writes are disabled in this device's configuration");
    if (typeof content !== 'string') throw new Error('content must be a string');
    const data = Buffer.from(content, 'utf8');
    if (data.length > MAX_FILE) throw new Error('Content exceeds 1 MiB');
    const previous = this.lock;
    let release;
    this.lock = new Promise((resolve) => { release = resolve; });
    await previous;
    try {
      const { fd, name } = await this.parent(arg, value);
      try {
        let current = null;
        try { current = await this._read(fd, name); } catch (error) {
          if (error?.code !== 'ENOENT' && !/not found|no such file/i.test(error?.message || '')) throw error;
        }
        if (current === null && expected_sha256) throw new Error('File no longer exists; read it again before updating');
        if (current !== null && sha256(current) !== expected_sha256) {
          throw new Error('Content conflict: supply the SHA-256 from read_file; empty hash means create only');
        }
        let mode = 0o600;
        if (current !== null) mode = (await asPromise(this.filesystem.statAt(fd, name))).mode & 0o777;
        await asPromise(this.filesystem.writeAtomic(fd, name, data, mode, current === null));
        return { path: value, size_bytes: data.length, sha256: sha256(data) };
      } finally { await this.close(fd); }
    } finally { release(); }
  }

  async edit(arg, value, old_text, new_text, expected_sha256) {
    const params = isParams(arg) ? arg : { root: arg, path: value, old_text, new_text, expected_sha256 };
    ({ root: arg, path: value, old_text, new_text, expected_sha256 } = params);
    if (!old_text) throw new Error('old_text cannot be empty');
    const { fd, name } = await this.parent(arg, value);
    let data;
    try {
      data = await this._read(fd, name);
      if (sha256(data) !== expected_sha256) throw new Error('Content conflict: read_file again');
      let text;
      try { text = new TextDecoder('utf-8', { fatal: true }).decode(data); }
      catch { throw new Error('File is not valid UTF-8'); }
      const first = text.indexOf(old_text);
      if (first < 0 || text.indexOf(old_text, first + old_text.length) >= 0) throw new Error('old_text must match exactly once');
      return await this.write(arg, value, text.slice(0, first) + new_text + text.slice(first + old_text.length), expected_sha256);
    } finally { await this.close(fd); }
  }

  async mkdir(arg, value) {
    const params = isParams(arg) ? arg : { root: arg, path: value };
    ({ root: arg, path: value } = params);
    if (!this.allowWrite) throw new Error('File writes are disabled');
    const { fd, name } = await this.parent(arg, value);
    try { await asPromise(this.filesystem.mkdirAt(fd, name)); }
    finally { await this.close(fd); }
    return { created: value };
  }

  async checkedDirectory(root, value = '.') {
    const [base, parts] = this.parts(root, value);
    const fd = await asPromise(this.filesystem.openDirectory(base, parts.length ? parts.join('/') : '.'));
    await this.close(fd);
    return path.join(base, ...parts);
  }

  async search(arg, query, value = '.', content = false, limit = 100) {
    const params = isParams(arg) ? arg : { root: arg, query, path: value, content, limit };
    ({ root: arg, query, path: value = '.', content = false, limit = 100 } = params);
    if (typeof query !== 'string' || !query || query.length > 512 || !Number.isInteger(limit) || limit < 1 || limit > 200) {
      throw new Error('Query must be 1..512 characters; limit 1..200');
    }
    const matches = [];
    let visited = 0; let truncated = false;
    const deadline = Date.now() + 3000;
    const walk = async (fd, prefix, depth) => {
      if (depth > MAX_SEARCH_DEPTH) { truncated = true; return; }
      let entries;
      try { entries = await asPromise(this.filesystem.listDirectory(fd)); } catch { return; }
      for (const entry of entries) {
        visited += 1;
        if (visited > MAX_SEARCH_VISITED || Date.now() > deadline || matches.length >= limit) { truncated = true; return; }
        const rel = `${prefix === '.' ? '' : `${prefix}/`}${entry.name}`;
        if (entry.type === 'symlink') continue;
        if (!content && casefold(entry.name).includes(casefold(query))) matches.push({ path: rel });
        if (entry.type === 'directory') {
          let child;
          try { child = await asPromise(this.filesystem.dupDirChild(fd, entry.name)); await walk(child, rel, depth + 1); }
          catch { /* inaccessible entries are skipped, matching Python */ }
          finally { if (child !== undefined) await this.close(child).catch(() => undefined); }
        } else if (content && entry.type === 'file') {
          try {
            const bytes = await this._read(fd, entry.name);
            const text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
            for (const [index, line] of text.split(/\r?\n/).entries()) {
              if (line.includes(query)) { matches.push({ path: rel, line: index + 1, text: line.slice(0, 500) }); if (matches.length >= limit) { truncated = true; return; } }
            }
          } catch { /* unreadable or non-UTF-8 files are skipped */ }
        }
      }
    };
    const fd = await this.directory(arg, value);
    try { await walk(fd, value === '.' ? '.' : value, 0); }
    finally { await this.close(fd); }
    return { matches: matches.slice(0, limit), visited, truncated };
  }
}

export default Files;
