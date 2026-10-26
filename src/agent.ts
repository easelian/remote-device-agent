import { StartupError, startupMode, type Startup } from './startup.js';
import os from 'node:os';
import { globalAgent as httpAgent, type IncomingMessage } from 'node:http';
import { globalAgent as httpsAgent } from 'node:https';
import type { DeviceConfig, SandboxSettings } from './types.js';
import { AuditWriteError, type AuditLog, type AuditRecord } from './audit.js';
import type { Policy } from './policy.js';
import type { TransportLimits } from './transport.js';
import { platformName } from './platform.js';
import { errorMessage, isRecord } from './boundary.js';
import { createHash } from 'node:crypto';
import { KNOWN_OPERATIONS, WRITE_OPERATIONS, PolicyDeniedError } from './policy.js';
import WebSocket from '../vendor/ws/lib/websocket.js';
import { Files } from './files.js';
import { Processes, ProcessStartError } from './processes.js';
import { SearchTasks } from './searches.js';
import { BoundedSender, TRANSPORT_LIMITS } from './transport.js';
import {
  OperationError,
  OperationReceipts,
  resultSummary,
  type ExecutionState,
} from './operation-receipts.js';

const pause = (milliseconds: number, signal?: AbortSignal) =>
  new Promise<void>((resolve) => {
    if (signal?.aborted) {
      return resolve();
    }
    const done = () => {
      clearTimeout(timer);
      signal?.removeEventListener('abort', done);
      resolve();
    };
    const timer = setTimeout(done, milliseconds);
    signal?.addEventListener('abort', done, { once: true });
  });

const FATAL_CLOSE_CODES = new Set([1008]);
const fatalError = (message: string) => Object.assign(new Error(message), { fatal: true });

// Runtime operation methods validate each field after the protocol envelope check.
type RpcParameters = Record<string, unknown>;
function operationFailure(error: unknown) {
  const value = isRecord(error) ? error.executionState : undefined;
  const state: ExecutionState =
    typeof value === 'string' &&
    ['not_sent', 'running', 'succeeded', 'failed', 'unknown'].includes(value)
      ? (value as ExecutionState)
      : 'unknown';
  const code =
    error instanceof StartupError ||
    error instanceof PolicyDeniedError ||
    error instanceof ProcessStartError ||
    error instanceof AuditWriteError ||
    error instanceof OperationError
      ? error.code
      : 'agent_error';
  return {
    error_code: code,
    execution_state: state,
    result_summary: resultSummary(isRecord(error) ? error.resultSummary : undefined),
  };
}
export interface AgentOptions {
  startup?: Pick<Startup, 'status' | 'set'>;
  policy?: Pick<Policy, 'describe' | 'denial'> | null;
  audit?:
    | (Pick<AuditLog, 'record' | 'close'> & Partial<Pick<AuditLog, 'stats' | 'required'>>)
    | null;
  inheritEnv?: readonly string[];
  sandbox?: SandboxSettings | null;
  transportLimits?: Partial<TransportLimits>;
  operationTimeoutMs?: number;
  shutdownTimeoutMs?: number;
  receiptLimits?: { maxRecords?: number; ttlMs?: number };
}
export interface AgentCloseResult {
  clean: boolean;
}
interface ResponseWaiter {
  cancel: () => void;
  timer: ReturnType<typeof setTimeout> | undefined;
}

export class Agent {
  readonly startup: AgentOptions['startup'];
  readonly config: DeviceConfig;
  readonly files: Files;
  readonly policy: AgentOptions['policy'];
  readonly audit: AgentOptions['audit'];
  readonly sandbox: SandboxSettings | null;
  readonly processes: Processes;
  readonly searches: SearchTasks;
  readonly inFlightOperations: Set<Promise<unknown>>;
  readonly transportLimits: TransportLimits;
  readonly operationTimeoutMs: number;
  readonly shutdownTimeoutMs: number;
  readonly receipts: OperationReceipts;
  private notifyReceipt: ((id: string) => Promise<void>) | null = null;
  private statusRequests = 0;
  closing: boolean;
  closeTask: Promise<AgentCloseResult> | null;
  cleanShutdown: boolean;
  constructor(config: DeviceConfig, options: AgentOptions = {}) {
    this.config = config;
    this.startup = options.startup;
    this.files = new Files(config.roots, config.allow_write);
    this.policy = options.policy ?? null;
    this.audit = options.audit ?? null;
    this.sandbox = options.sandbox ?? null;
    this.processes = new Processes(config, this.files, {
      sandbox: options.sandbox ?? null,
      inheritEnv: options.inheritEnv ?? [],
      onExit: (result, requestId) => {
        try {
          this.audit?.record({
            id: requestId,
            operation: 'process_exit',
            status: result.status,
            durationMs: Math.max(0, Date.now() - result.started_at * 1000),
            result,
            error: result.error ? JSON.stringify(result.error) : undefined,
          });
        } catch {
          console.error('Local audit could not record process completion; inspect audit health');
        }
      },
    });
    this.searches = new SearchTasks(this.files);
    this.inFlightOperations = new Set();
    this.transportLimits = { ...TRANSPORT_LIMITS, ...options.transportLimits };
    this.operationTimeoutMs = options.operationTimeoutMs ?? 20_000;
    this.shutdownTimeoutMs = options.shutdownTimeoutMs ?? 6000;
    this.receipts = new OperationReceipts(
      config.token,
      options.receiptLimits?.maxRecords,
      options.receiptLimits?.ttlMs,
    );
    this.closing = false;
    this.closeTask = null;
    this.cleanShutdown = false;
    for (const value of [this.operationTimeoutMs, this.shutdownTimeoutMs]) {
      if (!Number.isSafeInteger(value) || value < 1) {
        throw new Error('Invalid operation deadlines');
      }
    }
  }

  hardeningInfo() {
    const policy = this.policy?.describe() ?? {
      allowed_operations: null,
      readonly_roots: [],
      protected_paths: [],
    };
    const sandbox = this.sandbox;
    const effective = {
      effective_operations: KNOWN_OPERATIONS.filter(
        (operation) =>
          (!policy.allowed_operations || policy.allowed_operations.includes(operation)) &&
          (this.config.allow_write || !WRITE_OPERATIONS.includes(operation)) &&
          (this.config.allow_shell || operation !== 'start_process'),
      ),
      readonly_roots: Object.keys(this.files.roots)
        .filter((root) => !this.config.allow_write || policy.readonly_roots.includes(root))
        .sort(),
      protected_paths: policy.protected_paths,
      sandbox: {
        mode: sandbox?.mode ?? 'none',
        network: sandbox?.mode === 'bwrap' ? sandbox.network : true,
        hide_home: sandbox?.mode === 'bwrap' ? sandbox.hideHome : false,
        limits: sandbox?.limits ?? {},
      },
      inherit_env: [...this.processes.inheritEnv].sort(),
      local_audit: { enabled: this.audit !== null, required: this.audit?.required ?? false },
    };
    const revision = createHash('sha256')
      .update(
        JSON.stringify({
          ...effective,
          roots: this.files.roots,
          expose: sandbox?.expose ?? [],
          hiddenPaths: sandbox?.hiddenPaths ?? [],
          hiddenFiles: sandbox?.hiddenFiles ?? [],
        }),
      )
      .digest('hex');
    const paths = [];
    let bytes = 0;
    for (const entry of effective.protected_paths) {
      bytes += Buffer.byteLength(JSON.stringify(entry)) + 1;
      if (bytes > 2048) {
        break;
      }
      paths.push(entry);
    }
    return {
      version: 1,
      ...effective,
      local_audit: { ...effective.local_audit, ...this.audit?.stats?.() },
      protected_paths: paths,
      protected_paths_count: effective.protected_paths.length,
      protected_paths_truncated: paths.length !== effective.protected_paths.length,
      policy_revision: revision,
    };
  }

  info() {
    return {
      os: platformName(),
      release: os.release(),
      machine: os.machine(),
      hardening: this.hardeningInfo(),
      hostname: os.hostname(),
      roots: this.files.roots,
      allow_shell: this.config.allow_shell,
      allow_write: this.config.allow_write,
      max_processes: this.config.max_processes,
      max_process_seconds: this.config.max_process_seconds,
      shell: this.config.shell,
      file_limit_bytes: 1024 * 1024,
      process_output_limit_chars: 256 * 1024,
      shell_authority: 'Agent OS user; file roots do not sandbox shell commands',
      capabilities: {
        operation_receipts: {
          version: 1,
          instance_id: this.receipts.instanceId,
          max_records: this.receipts.maxRecords,
          ttl_seconds: this.receipts.ttlMs / 1000,
        },
        request_budget: 1,
        chunk_encoding: 1,
        process_tree_termination:
          process.platform === 'win32' ? 'taskkill-best-effort' : 'process-group',
        operations: [
          'device_info',
          'get_operation_status',
          ...(this.startup ? ['get_startup', 'set_startup'] : []),
          'list_directory',
          'read_file',
          'write_file',
          'edit_file',
          'make_directory',
          'search_files',
          'file_info',
          'read_file_chunk',
          'read_multiple_files',
          'move_file',
          'start_process',
          'read_process',
          'list_processes',
          'write_process_stdin',
          'terminate_process',
          'start_search',
          'get_search_results',
          'stop_search',
          'list_searches',
        ],
      },
      search_limits: {
        max_concurrent: 2,
        max_entries: 1000,
        max_depth: 30,
        max_visited: 10000,
        max_result_bytes: 256 * 1024,
        task_ttl_seconds: 30,
      },
      offline_process_grace_seconds: 300,
      transport_limits: {
        max_inflight_operations: 8,
        max_response_bytes: this.transportLimits.maxFrameBytes,
        max_buffered_bytes: this.transportLimits.maxBufferedBytes,
      },
      chunk_file_limit_bytes: 64 * 1024 * 1024,
    };
  }

  // Policy is evaluated first and every outcome (ok, error, denied) is audited.
  // Policy and audit are optional; without them dispatch behaves as before.
  async dispatch(
    operation: string,
    params: RpcParameters,
    context: { requestId?: string } = {},
  ): Promise<unknown> {
    const started = performance.now();
    const record = (status: string, extra: Pick<AuditRecord, 'result' | 'error'>) =>
      this.audit?.record({
        id: context.requestId,
        operation,
        status,
        params,
        durationMs: performance.now() - started,
        ...extra,
      });
    const denial = this.policy?.denial(operation, params);
    if (denial) {
      try {
        record('denied', { error: denial.message });
      } catch {
        throw new AuditWriteError(
          'Operation was rejected before execution; local audit failed',
          'not_sent',
        );
      }
      Object.assign(denial, { executionState: 'not_sent' });
      throw denial;
    }
    if (this.audit?.required) {
      try {
        record('started', {});
      } catch {
        if (operation !== 'device_info' && operation !== 'get_operation_status') {
          throw new AuditWriteError(
            'Operation was not executed because local audit failed',
            'not_sent',
          );
        }
      }
    }
    let result;
    try {
      result = await this.execute(operation, params, context);
    } catch (error) {
      const state = error instanceof ProcessStartError ? 'failed' : 'unknown';
      if (error instanceof Error) {
        Object.assign(error, { executionState: state });
      }
      try {
        record('error', { error: errorMessage(error) });
      } catch {
        throw new AuditWriteError(
          'Operation failed and its outcome could not be audited; inspect before retrying',
          'unknown',
        );
      }
      throw error;
    }
    try {
      record('ok', { result });
    } catch (error) {
      if (
        (operation === 'device_info' || operation === 'get_operation_status') &&
        error instanceof AuditWriteError
      ) {
        return result;
      }
      throw new AuditWriteError(
        'Operation completed but its outcome could not be audited; inspect before retrying',
        'succeeded',
        resultSummary(result),
      );
    }
    return result;
  }

  async execute(
    operation: string,
    params: RpcParameters,
    context: { requestId?: string } = {},
  ): Promise<unknown> {
    const files: Record<string, 'list' | 'read' | 'write' | 'edit' | 'mkdir' | 'search'> = {
      list_directory: 'list',
      read_file: 'read',
      write_file: 'write',
      edit_file: 'edit',
      make_directory: 'mkdir',
      search_files: 'search',
    };
    if (operation === 'device_info') {
      return this.info();
    }
    if (operation === 'get_operation_status') {
      return this.receipts.status(params.operation_id, params.instance_id);
    }
    if (operation === 'get_startup' || operation === 'set_startup') {
      if (!this.startup) {
        throw new Error('Startup management is unavailable for this Agent');
      }
      const keys = Object.keys(params);
      if (keys.some((key) => operation === 'get_startup' || key !== 'mode')) {
        throw new Error('Unknown startup parameter');
      }
      return operation === 'get_startup'
        ? this.startup.status()
        : this.startup.set(startupMode(params.mode));
    }
    const kind = files[operation];
    if (kind !== undefined) {
      return this.files[kind](params);
    }
    if (operation === 'file_info') {
      return this.files.fileInfo(params);
    }
    if (operation === 'read_file_chunk') {
      return this.files.readFileChunk(params);
    }
    if (operation === 'read_multiple_files') {
      return this.files.readMultipleFiles(params);
    }
    if (operation === 'move_file') {
      return this.files.moveFile(params);
    }
    const p = this.processes;
    switch (operation) {
      case 'start_process':
        return p.start(
          params.command,
          params.root,
          params.cwd,
          params.timeout_seconds,
          params.wait_seconds,
          context.requestId,
        );
      case 'read_process':
        return p.read(params.job_id, params.offset, params.limit);
      case 'list_processes':
        return p.list();
      case 'write_process_stdin':
        return p.writeStdin(params.job_id, params.text, params.close);
      case 'terminate_process':
        return p.terminate(params.job_id);
      case 'start_search':
        return this.searches.start_search(params);
      case 'get_search_results':
        return this.searches.get_search_results(params);
      case 'stop_search':
        return this.searches.stop_search(params);
      case 'list_searches':
        return this.searches.list_searches();
      default:
        throw new Error('Unknown operation');
    }
  }

  async session(
    signal?: AbortSignal,
    onReady: () => void = () => {},
    { preserveJobs = false } = {},
  ) {
    if (this.closing) {
      throw new Error('Agent is closing');
    }
    const url = new URL(this.config.gateway_url);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    url.pathname = '/agent/' + this.config.device_id;
    const ws = new WebSocket(url, {
      // ws otherwise supplies createConnection and bypasses Node's global agent.
      // Honor the process's local network policy without overriding its defaults.
      agent: url.protocol === 'wss:' ? httpsAgent : httpAgent,
      headers: { Authorization: 'Bearer ' + this.config.token },
      maxPayload: 2 * 1024 * 1024,
      handshakeTimeout: 15_000,
      perMessageDeflate: false,
      followRedirects: false,
    });
    const sender = new BoundedSender(ws, this.transportLimits);
    const activeIds = new Set<string>();
    const responseWaiters = new Set<ResponseWaiter>();
    let heartbeat: ReturnType<typeof setInterval> | undefined;
    let readyTimeout: ReturnType<typeof setTimeout> | undefined;
    let alive = true;
    let ready = false;
    let reason = 'network';
    let closeCode: number | null = null;
    let fatal = false;
    const stop = () => {
      sender.close();
      ws.terminate();
    };
    const send = async (value: Record<string, unknown>) => {
      try {
        await sender.send(value);
      } catch {
        stop();
      }
    };
    const notifyReceipt = async (id: string) => {
      if (ready && ws.readyState === 1) {
        await send({
          type: 'operation_result',
          ...this.receipts.status(id, this.receipts.instanceId),
        });
      }
    };
    if (signal?.aborted) {
      stop();
    }
    signal?.addEventListener('abort', stop, { once: true });
    try {
      await new Promise<void>((resolve, reject) => {
        ws.once('close', (code: number) => {
          closeCode = code;
          fatal ||= FATAL_CLOSE_CODES.has(code);
          if (fatal) {
            reason = 'revoked';
          }
          if (ready) {
            resolve();
          } else {
            reject(
              fatal
                ? fatalError('Gateway authorization was rejected')
                : new Error('Gateway handshake failed'),
            );
          }
        });
        ws.once('unexpected-response', (_request: unknown, response: IncomingMessage) => {
          const authorizationRejected = response.statusCode === 401 || response.statusCode === 403;
          if (authorizationRejected) {
            fatal = true;
            reason = 'revoked';
          }
          reject(
            authorizationRejected
              ? fatalError(`Gateway authorization rejected (HTTP ${response.statusCode})`)
              : new Error(`Gateway handshake failed (HTTP ${response.statusCode})`),
          );
          response.resume();
          stop();
        });
        ws.on('error', (error: unknown) => {
          if (!ready && !fatal) {
            const codes = (error instanceof AggregateError ? error.errors : [error])
              .filter(isRecord)
              .map((entry) => entry.code)
              .filter(
                (code): code is string =>
                  typeof code === 'string' && /^[A-Z][A-Z0-9_]{1,63}$/.test(code),
              );
            const detail = [...new Set(codes)].join(', ') || 'handshake or network timeout';
            reject(new Error(`Gateway connection failed (${detail})`));
          }
        });
        ws.on('pong', () => {
          alive = true;
        });
        ws.once('open', () => {
          send({ type: 'hello', version: 1, info: this.info() });
          readyTimeout = setTimeout(stop, 10_000);
        });
        ws.on('message', (raw: Buffer) => {
          let message: unknown;
          try {
            message = JSON.parse(raw.toString('utf8'));
          } catch {
            stop();
            return;
          }
          if (!isRecord(message)) {
            stop();
            return;
          }
          if (!ready) {
            if (message.type !== 'ready') {
              stop();
              return;
            }
            clearTimeout(readyTimeout);
            ready = true;
            this.notifyReceipt = notifyReceipt;
            this.processes.markOnline();
            onReady();
            void (async () => {
              for (const id of this.receipts.pending()) {
                if (ws.readyState !== 1) {
                  break;
                }
                await notifyReceipt(id);
              }
            })();
            heartbeat = setInterval(() => {
              if (!alive) {
                stop();
                return;
              }
              alive = false;
              ws.ping();
              send({ type: 'heartbeat' });
            }, 20_000);
            return;
          }
          if (message.type === 'heartbeat') {
            return;
          }
          if (message.type === 'operation_ack') {
            this.receipts.acknowledge(message.operation_id, message.instance_id);
            return;
          }
          const params = message.params ?? {};
          if (
            message.type !== 'request' ||
            typeof message.id !== 'string' ||
            message.id.length > 128 ||
            typeof message.operation !== 'string' ||
            message.operation.length > 64 ||
            !isRecord(params)
          ) {
            stop();
            return;
          }
          const requestId = message.id;
          const operationName = message.operation;
          if (this.closing) {
            void send({
              type: 'result',
              id: requestId,
              error: 'Device is shutting down',
              error_code: 'operation_busy',
              execution_state: 'not_sent',
            });
            return;
          }
          if (activeIds.has(requestId)) {
            stop();
            return;
          }
          const statusRequest = operationName === 'get_operation_status';
          if (statusRequest ? this.statusRequests >= 1 : this.inFlightOperations.size >= 8) {
            void send({
              type: 'result',
              id: requestId,
              error: 'Device busy',
              error_code: 'operation_busy',
              execution_state: 'not_sent',
            });
            return;
          }
          let budget = this.operationTimeoutMs;
          let receiptId: string | undefined;
          try {
            if (message.execution_budget_ms !== undefined) {
              if (
                typeof message.execution_budget_ms !== 'number' ||
                !Number.isSafeInteger(message.execution_budget_ms) ||
                message.execution_budget_ms < 1
              ) {
                throw new OperationError(
                  'operation_timeout',
                  'Request budget expired before execution',
                );
              }
              budget = Math.min(budget, message.execution_budget_ms);
            }
            if (message.operation_receipt !== undefined) {
              if (!isRecord(message.operation_receipt)) {
                throw new OperationError('operation_unavailable', 'Invalid operation receipt');
              }
              const receipt = message.operation_receipt;
              if (
                !this.receipts.begin(
                  receipt.operation_id,
                  receipt.instance_id,
                  operationName,
                  params,
                )
              ) {
                const existing = this.receipts.status(receipt.operation_id, receipt.instance_id);
                throw new OperationError(
                  'operation_unavailable',
                  'Operation already accepted; query its status',
                  existing.execution_state as ExecutionState,
                  existing.result_summary as Record<string, unknown>,
                );
              }
              receiptId = receipt.operation_id as string;
            }
          } catch (error) {
            void send({
              type: 'result',
              id: requestId,
              error: errorMessage(error).slice(0, 1000),
              ...operationFailure(error),
            });
            return;
          }
          if (statusRequest) {
            this.statusRequests += 1;
          }
          activeIds.add(requestId);
          const started = performance.now();
          let executionMs: number | undefined;
          const operation = Promise.resolve()
            .then(() => this.dispatch(operationName, params, { requestId }))
            .then(
              (result) => {
                if (receiptId) {
                  this.receipts.complete(receiptId, 'succeeded', result);
                }
                return result;
              },
              (error: unknown) => {
                const failure = operationFailure(error);
                if (receiptId) {
                  this.receipts.complete(
                    receiptId,
                    failure.execution_state,
                    failure.result_summary,
                    failure.error_code,
                  );
                }
                throw error;
              },
            )
            .finally(() => {
              executionMs = performance.now() - started;
              if (receiptId) {
                void this.notifyReceipt?.(receiptId);
              }
            });
          // The budget belongs to the Agent, so an unresolved I/O operation
          // still occupies a slot after a socket closes or its reply expires.
          this.inFlightOperations.add(operation);
          const completed = operation.finally(() => {
            this.inFlightOperations.delete(operation);
            activeIds.delete(requestId);
            if (statusRequest) {
              this.statusRequests -= 1;
            }
          });
          completed.catch(() => undefined);
          let operationTimer: ReturnType<typeof setTimeout> | undefined;
          let cancelResponse!: () => void;
          const expired = new Promise<never>((_, reject) => {
            cancelResponse = () =>
              reject(
                new OperationError(
                  'disconnected',
                  'Connection closed; operation outcome may be unknown. Inspect before retrying.',
                  'unknown',
                ),
              );
            operationTimer = setTimeout(
              () =>
                reject(
                  new OperationError(
                    'operation_timeout',
                    'Operation deadline exceeded; outcome may be unknown. Inspect before retrying.',
                    'unknown',
                  ),
                ),
              budget,
            );
          });
          const waiter = { cancel: cancelResponse, timer: operationTimer };
          responseWaiters.add(waiter);
          void Promise.race([completed, expired])
            .then(
              (result) =>
                send({ type: 'result', id: requestId, result, execution_ms: executionMs }),
              (error: unknown) =>
                send({
                  type: 'result',
                  id: requestId,
                  error: errorMessage(error).slice(0, 1000),
                  ...operationFailure(error),
                  ...(executionMs === undefined ? {} : { execution_ms: executionMs }),
                }),
            )
            .finally(() => {
              clearTimeout(operationTimer);
              responseWaiters.delete(waiter);
            });
        });
      });
    } finally {
      if (this.notifyReceipt === notifyReceipt) {
        this.notifyReceipt = null;
      }
      clearTimeout(readyTimeout);
      clearInterval(heartbeat);
      signal?.removeEventListener('abort', stop);
      for (const waiter of responseWaiters) {
        clearTimeout(waiter.timer);
        waiter.cancel();
      }
      responseWaiters.clear();
      if (signal?.aborted) {
        reason = 'aborted';
      }
      sender.close();
      ws.terminate();
      if (reason === 'network' && preserveJobs && !signal?.aborted && !fatal) {
        this.processes.markOffline();
      } else {
        await this.close();
      }
    }
    if (fatal) {
      throw fatalError(
        reason === 'revoked' ? 'Gateway authorization was revoked' : 'Gateway rejected Agent',
      );
    }
    return { reason, code: closeCode, fatal: false };
  }

  async run(signal?: AbortSignal): Promise<void> {
    let delay = 1000;
    try {
      while (!signal?.aborted) {
        try {
          const outcome = await this.session(
            signal,
            () => {
              delay = 1000;
              console.log(`Device ${this.config.device_id} connected`);
            },
            { preserveJobs: true },
          );
          if (!signal?.aborted) {
            console.error(
              `Gateway disconnected (${outcome.reason}, close=${outcome.code}); retrying in ${(delay / 1000).toFixed(1)}s`,
            );
          }
        } catch (error) {
          if (error && typeof error === 'object' && 'fatal' in error && error.fatal) {
            throw error;
          }
          if (!signal?.aborted) {
            console.error(`${errorMessage(error)}; retrying in ${(delay / 1000).toFixed(1)}s`);
          }
        }
        if (signal?.aborted) {
          break;
        }
        await pause(delay + Math.random() * 1000, signal);
        delay = Math.min(delay * 2, 30_000);
      }
    } finally {
      await this.close();
    }
  }

  async close(): Promise<AgentCloseResult> {
    if (this.closeTask) {
      return this.closeTask;
    }
    this.closing = true;
    this.closeTask = this.finishClose();
    return this.closeTask;
  }

  async finishClose(): Promise<AgentCloseResult> {
    const cleanup = Promise.allSettled([
      this.processes.close(),
      this.searches.close(),
      ...this.inFlightOperations,
    ]);
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<void>((resolve) => {
      timer = setTimeout(resolve, this.shutdownTimeoutMs);
    });
    await Promise.race([cleanup, deadline]);
    clearTimeout(timer);
    this.cleanShutdown =
      this.inFlightOperations.size === 0 &&
      this.searches.inFlight.size === 0 &&
      [...this.processes.jobs.values()].every((job) => job.closed);
    if (this.cleanShutdown) {
      this.audit?.close();
    }
    return { clean: this.cleanShutdown };
  }
}
