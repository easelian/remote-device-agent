import { moveParameters } from './file-params.js';
import { validateRelativePath } from './platform.js';
const ROOT_RE = /^[A-Za-z0-9_-]{1,64}$/;
const MAX_PROTECTED = 64;

export const WRITE_OPERATIONS = Object.freeze(['write_file', 'edit_file', 'make_directory', 'move_file']);
export const KNOWN_OPERATIONS = Object.freeze(['device_info', 'list_directory', 'read_file', 'write_file', 'edit_file',
  'make_directory', 'search_files', 'file_info', 'read_file_chunk', 'read_multiple_files', 'move_file',
  'start_process', 'read_process', 'list_processes', 'write_process_stdin', 'terminate_process',
  'start_search', 'get_search_results', 'stop_search', 'list_searches']);

const writeOperations = new Set(WRITE_OPERATIONS);
const knownOperations = new Set(KNOWN_OPERATIONS);
const fold = (value: string): string => (['darwin', 'win32'].includes(process.platform) ? value.toLowerCase() : value);
const components = (value: string): string[] => validateRelativePath(value).map(fold);

function validateList(value: unknown, label: string, limit: number): string[] {
  if (!Array.isArray(value) || value.length > limit || value.some((item) => typeof item !== 'string')) {
    throw new Error(`${label} must be an array of at most ${limit} strings`);
  }
  return value;
}

// File-tool policy. It is evaluated before an operation reaches Files, and
// it deliberately does NOT constrain shell commands: a shell runs as the
// Agent OS user. Use the sandbox settings to contain start_process.
export class PolicyDeniedError extends Error {
  readonly code: string;
  constructor(code: string, message: string) { super(message); this.name = 'PolicyDeniedError'; this.code = code; }
}

export interface PolicyOptions {
  allowed_operations?: string[] | null;
  readonly_roots?: string[];
  protected_paths?: string[];
}

export class Policy {
  readonly allowed: Set<string> | null;
  readonly readonlyRoots: Set<string>;
  readonly protected: string[][];
  constructor({ allowed_operations: allowed = null, readonly_roots: readonlyRoots = [], protected_paths: protectedPaths = [] }: PolicyOptions = {}) {
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
      let parts: string[];
      try { parts = components(entry); }
      catch { throw new Error('Invalid protected_paths entry'); }
      if (!parts.length || parts.includes('..')) throw new Error('Invalid protected_paths entry');
      return parts;
    });
  }

  // Returns a denial message, or null when the operation may proceed.
  check(operation: string, params: unknown) { return this.denial(operation, params)?.message ?? null; }

  denial(operation: string, params: unknown) {
    if (this.allowed && !this.allowed.has(operation)) return new PolicyDeniedError('policy_operation_denied', `Policy denied: operation ${operation} is not allowed on this device`);
    if (!writeOperations.has(operation) || !params || typeof params !== 'object') return null;
    const targets: Array<{ root: unknown; path: unknown }> = [];
    if (operation === 'move_file') {
      let move;
      try { move = moveParameters(params); }
      catch (error) { return new PolicyDeniedError('policy_parameters_invalid', `Policy denied: ${error instanceof Error ? error.message : String(error)}`); }
      targets.push({ root: move.root, path: move.path });
      targets.push({ root: move.destination_root, path: move.destination_path });
    } else {
      const values = params as Record<string, unknown>;
      targets.push({ root: values.root, path: values.path });
    }
    let checkedTargets: Array<{ root: unknown; parts: string[] | null }>;
    try {
      checkedTargets = targets.map(({ root, path: target }) => ({ root,
        parts: typeof target === 'string' ? components(target) : null }));
    } catch (error) {
      return new PolicyDeniedError('policy_parameters_invalid', `Policy denied: ${error instanceof Error ? error.message : String(error)}`);
    }
    for (const { root, parts } of checkedTargets) {
      if (typeof root === 'string' && this.readonlyRoots.has(root)) return new PolicyDeniedError('policy_root_readonly', `Policy denied: root ${root} is read-only`);
      if (parts === null) continue;
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
