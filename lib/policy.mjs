import { moveParameters } from './file-params.mjs';
const ROOT_RE = /^[A-Za-z0-9_-]{1,64}$/;
const MAX_PROTECTED = 64;

export const WRITE_OPERATIONS = Object.freeze(['write_file', 'edit_file', 'make_directory', 'move_file']);
export const KNOWN_OPERATIONS = Object.freeze(['device_info', 'list_directory', 'read_file', 'write_file', 'edit_file',
  'make_directory', 'search_files', 'file_info', 'read_file_chunk', 'read_multiple_files', 'move_file',
  'start_process', 'read_process', 'list_processes', 'write_process_stdin', 'terminate_process',
  'start_search', 'get_search_results', 'stop_search', 'list_searches']);

const writeOperations = new Set(WRITE_OPERATIONS);
const knownOperations = new Set(KNOWN_OPERATIONS);
const fold = (value) => (process.platform === 'darwin' ? value.toLowerCase() : value);
const components = (value) => value.split('/').filter((part) => part && part !== '.').map(fold);

function validateList(value, label, limit) {
  if (!Array.isArray(value) || value.length > limit || value.some((item) => typeof item !== 'string')) {
    throw new Error(`${label} must be an array of at most ${limit} strings`);
  }
  return value;
}

// File-tool policy. It is evaluated before an operation reaches Files, and
// it deliberately does NOT constrain shell commands: a shell runs as the
// Agent OS user. Use the sandbox settings to contain start_process.
export class PolicyDeniedError extends Error {
  constructor(code, message) { super(message); this.name = 'PolicyDeniedError'; this.code = code; }
}

export class Policy {
  constructor({ allowed_operations: allowed = null, readonly_roots: readonlyRoots = [], protected_paths: protectedPaths = [] } = {}) {
    if (allowed !== null) {
      validateList(allowed, 'allowed_operations', KNOWN_OPERATIONS.length);
      for (const operation of allowed) if (!knownOperations.has(operation)) throw new Error(`Unknown operation in allowed_operations: ${operation}`);
    }
    validateList(readonlyRoots, 'readonly_roots', 64);
    for (const root of readonlyRoots) if (!ROOT_RE.test(root)) throw new Error('Invalid root name in readonly_roots');
    validateList(protectedPaths, 'protected_paths', MAX_PROTECTED);
    this.allowed = allowed === null ? null : new Set([...allowed, 'device_info']);
    this.readonlyRoots = new Set(readonlyRoots);
    this.protected = protectedPaths.map((entry) => {
      if (!entry || entry.length > 256 || entry.includes('\0') || entry.startsWith('/')) throw new Error('Invalid protected_paths entry');
      const parts = components(entry);
      if (!parts.length || parts.includes('..')) throw new Error('Invalid protected_paths entry');
      return parts;
    });
  }

  // Returns a denial message, or null when the operation may proceed.
  check(operation, params) { return this.denial(operation, params)?.message ?? null; }

  denial(operation, params) {
    if (this.allowed && !this.allowed.has(operation)) return new PolicyDeniedError('policy_operation_denied', `Policy denied: operation ${operation} is not allowed on this device`);
    if (!writeOperations.has(operation) || !params || typeof params !== 'object') return null;
    const targets = [];
    if (operation === 'move_file') {
      let move;
      try { move = moveParameters(params); }
      catch (error) { return new PolicyDeniedError('policy_parameters_invalid', `Policy denied: ${error.message}`); }
      targets.push({ root: move.root, path: move.path });
      targets.push({ root: move.destination_root, path: move.destination_path });
    } else targets.push({ root: params.root, path: params.path });
    for (const { root, path: target } of targets) {
      if (typeof root === 'string' && this.readonlyRoots.has(root)) return new PolicyDeniedError('policy_root_readonly', `Policy denied: root ${root} is read-only`);
      if (typeof target !== 'string') continue;
      const parts = components(target);
      for (const protectedParts of this.protected) {
        for (let start = 0; start + protectedParts.length <= parts.length; start++) {
          if (protectedParts.every((part, index) => parts[start + index] === part)) {
            return new PolicyDeniedError('policy_path_protected', `Policy denied: ${protectedParts.join('/')} is a protected path`);
          }
        }
      }
    }
    return null;
  }

  describe() {
    return { allowed_operations: this.allowed ? [...this.allowed].sort() : null,
      readonly_roots: [...this.readonlyRoots].sort(), protected_paths: this.protected.map((parts) => parts.join('/')) };
  }
}
