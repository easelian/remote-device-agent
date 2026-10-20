import { createHash, randomBytes } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

export const AUDIT_DEFAULTS = Object.freeze({ maxBytes: 5 * 1024 * 1024, maxFiles: 3 });

const MAX_FIELD = 512;
const MAX_LIST = 16;
const MAX_ERROR = 300;
const MAX_LINE = 16 * 1024;
// Free-form strings that may carry secrets are never written verbatim:
// only their size and digest are kept, which still lets an investigator
// confirm "this exact content was written" without storing the content.
const DIGEST_ONLY_KEYS = new Set(['content', 'old_text', 'new_text', 'text', 'query']);
const PLAIN_STRING_KEYS = new Set(['root', 'path', 'cwd', 'source', 'destination', 'destination_root',
  'destination_path', 'job_id', 'search_id', 'expected_sha256']);

const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const clip = (value, limit = MAX_FIELD) => (value.length > limit ? `${value.slice(0, limit)}…` : value);

// Default-deny summary of request parameters: unknown string fields are
// reduced to their length so a new parameter can never leak into the log.
export function summarizeParams(params) {
  const out = {};
  if (!params || typeof params !== 'object' || Array.isArray(params)) return out;
  for (const [key, value] of Object.entries(params)) {
    if (key === 'command' && typeof value === 'string') {
      out.command_bytes = Buffer.byteLength(value);
      out.command_sha256 = sha256(value);
      out.command_chars = Array.from(value).length;
    } else if (DIGEST_ONLY_KEYS.has(key) && typeof value === 'string') {
      out[key] = { bytes: Buffer.byteLength(value), sha256: sha256(value) };
    } else if (typeof value === 'boolean' || typeof value === 'number') {
      out[key] = value;
    } else if (typeof value === 'string' && PLAIN_STRING_KEYS.has(key)) {
      out[key] = clip(value);
    } else if (key === 'paths' && Array.isArray(value)) {
      out.paths = value.slice(0, MAX_LIST).map((item) => (typeof item === 'string' ? clip(item, 256) : null));
      if (value.length > MAX_LIST) out.paths_total = value.length;
    } else if (key === 'files' && Array.isArray(value)) {
      out.files = value.slice(0, MAX_LIST).map((item) => ({
        root: typeof item?.root === 'string' ? clip(item.root, 64) : null,
        path: typeof item?.path === 'string' ? clip(item.path, 256) : null }));
      if (value.length > MAX_LIST) out.files_total = value.length;
    } else if (typeof value === 'string') {
      out[key] = { chars: Array.from(value).length };
    } else {
      out[key] = { type: Array.isArray(value) ? 'array' : typeof value };
    }
  }
  return out;
}

// Results are summarised by an allow-list; file contents and process output
// are never copied into the audit log.
export function summarizeResult(result) {
  if (!result || typeof result !== 'object') return undefined;
  const out = {};
  for (const key of ['size_bytes', 'sha256', 'exit_code', 'status', 'job_id', 'pid', 'total', 'truncated',
    'moved', 'created', 'sent', 'stdin_closed', 'total_bytes', 'visited', 'search_id']) {
    const value = result[key];
    if (['string', 'number', 'boolean'].includes(typeof value) || value === null) out[key] = typeof value === 'string' ? clip(value, 128) : value;
  }
  for (const key of ['entries', 'matches', 'files', 'jobs', 'searches']) {
    if (Array.isArray(result[key])) out[`${key}_count`] = result[key].length;
  }
  return Object.keys(out).length ? out : undefined;
}

function openAppend(file) {
  const flags = fs.constants.O_WRONLY | fs.constants.O_APPEND | fs.constants.O_CREAT | fs.constants.O_NOFOLLOW;
  const fd = fs.openSync(file, flags, 0o600);
  try {
    const stat = fs.fstatSync(fd);
    const uid = typeof process.getuid === 'function' ? process.getuid() : stat.uid;
    if (!stat.isFile() || stat.uid !== uid || (stat.mode & 0o077)) {
      throw new Error('Audit log must be a regular file owned by this user with mode 0600');
    }
    return { fd, size: stat.size };
  } catch (error) { fs.closeSync(fd); throw error; }
}

function ensurePrivateDirectory(directory) {
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  const stat = fs.lstatSync(directory);
  const uid = typeof process.getuid === 'function' ? process.getuid() : stat.uid;
  if (!stat.isDirectory() || stat.isSymbolicLink() || stat.uid !== uid || (stat.mode & 0o077)) {
    throw new Error('Audit directory must be a real directory owned by this user with mode 0700');
  }
}

// Append-only JSONL audit trail. Recording is best effort by design: an audit
// failure must not turn into a denial of service for the device, but it is
// counted in stats() so that silent loss is detectable.
export class AuditLog {
  constructor({ directory, name = 'agent', maxBytes = AUDIT_DEFAULTS.maxBytes, maxFiles = AUDIT_DEFAULTS.maxFiles } = {}) {
    if (typeof directory !== 'string' || !path.isAbsolute(directory)) throw new TypeError('Audit directory must be an absolute path');
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(name)) throw new TypeError('Invalid audit log name');
    if (!Number.isSafeInteger(maxBytes) || maxBytes < 4096 || !Number.isSafeInteger(maxFiles) || maxFiles < 1 || maxFiles > 20) {
      throw new TypeError('Invalid audit log limits');
    }
    this.directory = directory;
    this.file = path.join(directory, `${name}.jsonl`);
    this.maxBytes = maxBytes;
    this.maxFiles = maxFiles;
    this.boot = randomBytes(4).toString('hex');
    this.sequence = 0;
    this.previous = '0'.repeat(16);
    this.written = 0;
    this.dropped = 0;
    this.lastError = null;
    this.closed = false;
    this.rollbackSize = null;
    ensurePrivateDirectory(directory);
    ({ fd: this.fd, size: this.size } = openAppend(this.file));
  }

  record({ id, operation, status, durationMs, params, result, error }) {
    try {
      if (this.closed) throw new Error('Audit log is closed');
      if (this.fd < 0) ({ fd: this.fd, size: this.size } = openAppend(this.file));
      this.repairTail();
      const entry = { ts: new Date().toISOString(), boot: this.boot, seq: ++this.sequence, prev: this.previous,
        id: typeof id === 'string' ? clip(id, 128) : undefined, op: clip(String(operation), 64), status,
        ms: Math.round(durationMs ?? 0), params: summarizeParams(params), result: summarizeResult(result),
        error: error ? clip(String(error), MAX_ERROR) : undefined };
      let line = `${JSON.stringify(entry)}\n`;
      const limit = Math.min(MAX_LINE, this.maxBytes);
      if (Buffer.byteLength(line) > limit) {
        entry.params = { truncated: true };
        line = `${JSON.stringify(entry)}\n`;
      }
      if (Buffer.byteLength(line) > limit) {
        entry.result = { truncated: true };
        entry.error = error ? { sha256: sha256(String(error)), bytes: Buffer.byteLength(String(error)) } : undefined;
        line = `${JSON.stringify(entry)}\n`;
      }
      if (Buffer.byteLength(line) > limit) throw new Error('Audit metadata exceeds record limit');
      const data = Buffer.from(line);
      if (this.size + data.length > this.maxBytes) this.rotate();
      this.rollbackSize = this.size;
      let offset = 0;
      while (offset < data.length) {
        const written = fs.writeSync(this.fd, data, offset, data.length - offset);
        if (!Number.isInteger(written) || written <= 0 || written > data.length - offset) {
          throw new Error('Audit write made no progress');
        }
        offset += written;
      }
      this.rollbackSize = null;
      this.size += data.length;
      this.previous = sha256(line).slice(0, 16);
      this.written += 1;
    } catch (failure) {
      this.dropped += 1;
      this.lastError = failure?.code ?? 'ERROR';
      // Remove a partial record before any later append. If recovery itself
      // fails, keep the offset and retry before accepting another record.
      try { this.repairTail(); } catch { /* next record retries repair */ }
    }
  }

  repairTail() {
    if (this.rollbackSize === null) return;
    fs.ftruncateSync(this.fd, this.rollbackSize);
    this.size = this.rollbackSize;
    this.rollbackSize = null;
  }

  rotate() {
    if (this.fd >= 0) fs.closeSync(this.fd);
    this.fd = -1;
    try {
      if (this.maxFiles === 1) fs.rmSync(this.file, { force: true });
      else {
        fs.rmSync(`${this.file}.${this.maxFiles - 1}`, { force: true });
        for (let index = this.maxFiles - 2; index >= 1; index--) {
          if (fs.existsSync(`${this.file}.${index}`)) fs.renameSync(`${this.file}.${index}`, `${this.file}.${index + 1}`);
        }
        fs.renameSync(this.file, `${this.file}.1`);
      }
    } finally {
      ({ fd: this.fd, size: this.size } = openAppend(this.file));
    }
  }

  stats() { return { written: this.written, dropped: this.dropped, last_error: this.lastError }; }

  close() {
    this.closed = true;
    if (this.fd >= 0) { try { fs.closeSync(this.fd); } catch { /* already closed */ } this.fd = -1; }
  }
}
