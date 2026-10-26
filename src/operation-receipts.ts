import { createHmac, randomBytes } from 'node:crypto';

export const RECEIPT_OPERATIONS = new Set([
  'write_file',
  'edit_file',
  'make_directory',
  'move_file',
  'start_process',
  'write_process_stdin',
  'terminate_process',
  'start_search',
  'stop_search',
]);

export type ExecutionState = 'not_sent' | 'running' | 'succeeded' | 'failed' | 'unknown';

export class OperationError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly executionState: ExecutionState = 'not_sent',
    readonly resultSummary: Record<string, unknown> = {},
  ) {
    super(message);
    this.name = 'OperationError';
  }
}

export function resultSummary(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return {};
  }
  const source = value as Record<string, unknown>;
  const result: Record<string, unknown> = {};
  for (const key of ['job_id', 'search_id']) {
    const item = source[key];
    if (typeof item === 'string' && /^[A-Za-z0-9_-]{1,128}$/.test(item)) {
      result[key] = item;
    }
  }
  if (typeof source.sha256 === 'string' && /^[0-9a-f]{64}$/.test(source.sha256)) {
    result.sha256 = source.sha256;
  }
  for (const key of ['size_bytes', 'sent']) {
    const item = source[key];
    if (typeof item === 'number' && Number.isSafeInteger(item) && item >= 0) {
      result[key] = item;
    }
  }
  if (
    source.exit_code === null ||
    (typeof source.exit_code === 'number' &&
      Number.isInteger(source.exit_code) &&
      source.exit_code >= -(2 ** 31) &&
      source.exit_code < 2 ** 31)
  ) {
    result.exit_code = source.exit_code;
  }
  if (
    typeof source.status === 'string' &&
    [
      'running',
      'exited',
      'failed',
      'timed_out',
      'terminated',
      'queued',
      'completed',
      'stopped',
    ].includes(source.status)
  ) {
    result.status = source.status;
  }
  if (typeof source.stdin_closed === 'boolean') {
    result.stdin_closed = source.stdin_closed;
  }
  return result;
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(canonical);
  }
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value)
        .sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0))
        .map(([key, item]) => [key, canonical(item)]),
    );
  }
  return value;
}

interface Receipt {
  operationId: string;
  operation: string;
  fingerprint: string;
  state: ExecutionState;
  result: Record<string, unknown>;
  errorCode?: string;
  settled: boolean;
  acknowledged: boolean;
  expires: number;
}

/** Metadata survives a socket reconnect, but never pretends to survive a process restart. */
export class OperationReceipts {
  readonly instanceId = randomBytes(16).toString('hex');
  private readonly records = new Map<string, Receipt>();

  constructor(
    private readonly token: string,
    readonly maxRecords = 1024,
    readonly ttlMs = 3600_000,
  ) {
    if (
      !Number.isSafeInteger(maxRecords) ||
      maxRecords < 1 ||
      maxRecords > 100_000 ||
      !Number.isSafeInteger(ttlMs) ||
      ttlMs < 1 ||
      ttlMs > 86400_000
    ) {
      throw new Error('Invalid operation receipt limits');
    }
  }

  private prune() {
    const now = performance.now();
    for (const [id, record] of this.records) {
      if (record.settled && record.expires <= now) {
        this.records.delete(id);
      }
    }
  }

  private identifier(id: unknown): asserts id is string {
    if (typeof id !== 'string' || !/^[0-9a-f]{32}$/.test(id)) {
      throw new OperationError(
        'operation_unavailable',
        'Operation receipt is unavailable',
        'unknown',
      );
    }
  }

  begin(id: unknown, instance: unknown, operation: string, params: Record<string, unknown>) {
    this.identifier(id);
    if (instance !== this.instanceId || !RECEIPT_OPERATIONS.has(operation)) {
      throw new OperationError(
        'operation_instance_changed',
        'Operation belongs to another Agent instance',
      );
    }
    this.prune();
    const fingerprint = createHmac('sha256', this.token)
      .update(JSON.stringify(canonical([operation, params])))
      .digest('hex');
    const previous = this.records.get(id);
    if (previous) {
      if (previous.fingerprint !== fingerprint) {
        throw new OperationError(
          'operation_conflict',
          'Operation ID was used with different parameters',
        );
      }
      return false;
    }
    if (this.records.size >= this.maxRecords) {
      throw new OperationError('operation_capacity', 'Operation receipt capacity is busy');
    }
    this.records.set(id, {
      operationId: id,
      operation,
      fingerprint,
      state: 'running',
      result: {},
      settled: false,
      acknowledged: false,
      expires: performance.now() + this.ttlMs,
    });
    return true;
  }

  complete(id: string, state: ExecutionState, result?: unknown, errorCode?: string) {
    const record = this.records.get(id);
    if (!record || record.settled) {
      return;
    }
    record.state = state;
    record.result = resultSummary(result);
    if (errorCode) {
      record.errorCode = errorCode;
    } else {
      delete record.errorCode;
    }
    record.settled = true;
    record.expires = performance.now() + this.ttlMs;
  }

  status(id: unknown, instance: unknown): Record<string, unknown> {
    this.identifier(id);
    this.prune();
    const record = instance === this.instanceId ? this.records.get(id) : undefined;
    return {
      operation_id: id,
      instance_id: this.instanceId,
      execution_state: record?.state ?? 'unknown',
      result_summary: record ? { ...record.result } : {},
      ...(record?.errorCode ? { error_code: record.errorCode } : {}),
    };
  }

  pending(): string[] {
    this.prune();
    return [...this.records.values()]
      .filter((record) => record.settled && !record.acknowledged)
      .map((record) => record.operationId);
  }

  acknowledge(id: unknown, instance: unknown) {
    if (typeof id === 'string' && instance === this.instanceId) {
      const record = this.records.get(id);
      if (record?.settled) {
        record.acknowledged = true;
      }
    }
  }
}
