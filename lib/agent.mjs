import os from 'node:os';
import WebSocket from '../vendor/ws/lib/websocket.js';
import { Files } from './files.mjs';
import { Processes } from './processes.mjs';
import { SearchTasks } from './searches.mjs';
import { BoundedSender, TRANSPORT_LIMITS } from './transport.mjs';

const pause = (milliseconds, signal) => new Promise((resolve) => {
  if (signal?.aborted) return resolve();
  const done = () => { clearTimeout(timer); signal?.removeEventListener('abort', done); resolve(); };
  const timer = setTimeout(done, milliseconds);
  signal?.addEventListener('abort', done, { once: true });
});

const FATAL_CLOSE_CODES = new Set([1008]);
const fatalError = (message) => Object.assign(new Error(message), { fatal: true });

export class Agent {
  constructor(config, options = {}) {
    this.config = config;
    this.files = new Files(config.roots, config.allow_write);
    this.processes = new Processes(config, this.files);
    this.searches = new SearchTasks(this.files);
    this.inFlightOperations = new Set();
    this.transportLimits = { ...TRANSPORT_LIMITS, ...options.transportLimits };
    this.operationTimeoutMs = options.operationTimeoutMs ?? 20_000;
    this.shutdownTimeoutMs = options.shutdownTimeoutMs ?? 6000;
    this.closing = false;
    this.closeTask = null;
    this.cleanShutdown = false;
    for (const value of [this.operationTimeoutMs, this.shutdownTimeoutMs]) {
      if (!Number.isSafeInteger(value) || value < 1) throw new Error('Invalid operation deadlines');
    }
  }

  info() {
    return { os: process.platform === 'darwin' ? 'Darwin' : 'Linux', release: os.release(), machine: os.machine(),
      hostname: os.hostname(), roots: this.files.roots, allow_shell: this.config.allow_shell,
      allow_write: this.config.allow_write, max_processes: this.config.max_processes,
      max_process_seconds: this.config.max_process_seconds, shell: this.config.shell,
      file_limit_bytes: 1024 * 1024, process_output_limit_chars: 256 * 1024,
      shell_authority: 'Agent OS user; file roots do not sandbox shell commands',
      capabilities: { operations: ['device_info', 'list_directory', 'read_file', 'write_file', 'edit_file',
        'make_directory', 'search_files', 'file_info', 'read_file_chunk', 'read_multiple_files', 'move_file',
        'start_process', 'read_process', 'list_processes', 'write_process_stdin', 'terminate_process',
        'start_search', 'get_search_results', 'stop_search', 'list_searches'] },
      search_limits: { max_concurrent: 2, max_entries: 1000, max_depth: 30, max_visited: 10000,
        max_result_bytes: 256 * 1024, task_ttl_seconds: 30 },
      offline_process_grace_seconds: 300,
      transport_limits: { max_inflight_operations: 8, max_response_bytes: this.transportLimits.maxFrameBytes,
        max_buffered_bytes: this.transportLimits.maxBufferedBytes },
      chunk_file_limit_bytes: 64 * 1024 * 1024 };
  }

  async dispatch(operation, params) {
    const files = { list_directory: 'list', read_file: 'read', write_file: 'write', edit_file: 'edit',
      make_directory: 'mkdir', search_files: 'search' };
    if (operation === 'device_info') return this.info();
    if (Object.hasOwn(files, operation)) return this.files[files[operation]](params);
    if (operation === 'file_info') return this.files.fileInfo(params);
    if (operation === 'read_file_chunk') return this.files.readFileChunk(params);
    if (operation === 'read_multiple_files') return this.files.readMultipleFiles(params);
    if (operation === 'move_file') return this.files.moveFile(params);
    const p = this.processes;
    switch (operation) {
      case 'start_process': return p.start(params.command, params.root, params.cwd, params.timeout_seconds, params.wait_seconds);
      case 'read_process': return p.read(params.job_id, params.offset, params.limit);
      case 'list_processes': return p.list();
      case 'write_process_stdin': return p.writeStdin(params.job_id, params.text, params.close);
      case 'terminate_process': return p.terminate(params.job_id);
      case 'start_search': return this.searches.start_search(params);
      case 'get_search_results': return this.searches.get_search_results(params);
      case 'stop_search': return this.searches.stop_search(params);
      case 'list_searches': return this.searches.list_searches();
      default: throw new Error('Unknown operation');
    }
  }

  async session(signal, onReady = () => {}, { preserveJobs = false } = {}) {
    if (this.closing) throw new Error('Agent is closing');
    const url = new URL(this.config.gateway_url);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    url.pathname = '/agent/' + this.config.device_id;
    const ws = new WebSocket(url, { headers: { Authorization: 'Bearer ' + this.config.token },
      maxPayload: 2 * 1024 * 1024, handshakeTimeout: 15_000, perMessageDeflate: false, followRedirects: false });
    const sender = new BoundedSender(ws, this.transportLimits);
    const activeIds = new Set();
    const responseWaiters = new Set();
    let heartbeat; let readyTimeout; let alive = true; let ready = false; let reason = 'network';
    let closeCode = null;
    let fatal = false;
    const stop = () => { sender.close(); ws.terminate(); };
    const send = async (value) => { try { await sender.send(value); } catch { stop(); } };
    if (signal?.aborted) stop();
    signal?.addEventListener('abort', stop, { once: true });
    try {
      await new Promise((resolve, reject) => {
        ws.once('close', (code) => {
          closeCode = code;
          fatal ||= FATAL_CLOSE_CODES.has(code);
          if (fatal) reason = 'revoked';
          if (ready) resolve();
          else reject(fatal ? fatalError('Gateway authorization was rejected') : new Error('Gateway handshake failed'));
        });
        ws.once('unexpected-response', (_request, response) => {
          const authorizationRejected = response.statusCode === 401 || response.statusCode === 403;
          if (authorizationRejected) { fatal = true; reason = 'revoked'; }
          reject(authorizationRejected ? fatalError(`Gateway authorization rejected (HTTP ${response.statusCode})`)
            : new Error(`Gateway handshake failed (HTTP ${response.statusCode})`));
          response.resume(); stop();
        });
        ws.on('error', () => { if (!ready && !fatal) reject(new Error('Gateway connection failed')); });
        ws.on('pong', () => { alive = true; });
        ws.once('open', () => {
          send({ type: 'hello', version: 1, info: this.info() });
          readyTimeout = setTimeout(stop, 10_000);
        });
        ws.on('message', (raw) => {
          let message;
          try { message = JSON.parse(raw.toString('utf8')); }
          catch { stop(); return; }
          if (!message || typeof message !== 'object' || Array.isArray(message)) { stop(); return; }
          if (!ready) {
            if (message.type !== 'ready') { stop(); return; }
            clearTimeout(readyTimeout); ready = true; this.processes.markOnline(); onReady();
            heartbeat = setInterval(() => {
              if (!alive) { stop(); return; }
              alive = false; ws.ping(); send({ type: 'heartbeat' });
            }, 20_000);
            return;
          }
          if (message.type === 'heartbeat') return;
          const params = message.params ?? {};
          if (message.type !== 'request' || typeof message.id !== 'string' || message.id.length > 128 ||
              typeof message.operation !== 'string' || message.operation.length > 64 ||
              !params || typeof params !== 'object' || Array.isArray(params)) { stop(); return; }
          if (this.closing) {
            void send({ type: 'result', id: message.id, error: 'Device is shutting down' });
            return;
          }
          if (activeIds.has(message.id)) { stop(); return; }
          if (this.inFlightOperations.size >= 8) { void send({ type: 'result', id: message.id, error: 'Device busy' }); return; }
          activeIds.add(message.id);
          const operation = Promise.resolve().then(() => this.dispatch(message.operation, params));
          // The budget belongs to the Agent, so an unresolved I/O operation
          // still occupies a slot after a socket closes or its reply expires.
          this.inFlightOperations.add(operation);
          const completed = operation.finally(() => {
            this.inFlightOperations.delete(operation);
            activeIds.delete(message.id);
          });
          completed.catch(() => undefined);
          let operationTimer;
          let cancelResponse;
          const expired = new Promise((_, reject) => {
            cancelResponse = () => reject(new Error('Connection closed; operation outcome may be unknown. Inspect before retrying.'));
            operationTimer = setTimeout(() => reject(new Error('Operation deadline exceeded; outcome may be unknown. Inspect before retrying.')),
              this.operationTimeoutMs);
          });
          const waiter = { cancel: cancelResponse, timer: operationTimer };
          responseWaiters.add(waiter);
          void Promise.race([completed, expired]).then(
            (result) => send({ type: 'result', id: message.id, result }),
            (error) => send({ type: 'result', id: message.id, error: String(error.message).slice(0, 1000) }))
            .finally(() => { clearTimeout(operationTimer); responseWaiters.delete(waiter); });
        });
      });
    } finally {
      clearTimeout(readyTimeout); clearInterval(heartbeat); signal?.removeEventListener('abort', stop);
      for (const waiter of responseWaiters) { clearTimeout(waiter.timer); waiter.cancel(); }
      responseWaiters.clear();
      if (signal?.aborted) reason = 'aborted';
      sender.close(); ws.terminate();
      if (reason === 'network' && preserveJobs && !signal?.aborted && !fatal) this.processes.markOffline();
      else await this.close();
    }
    if (fatal) throw fatalError(reason === 'revoked' ? 'Gateway authorization was revoked' : 'Gateway rejected Agent');
    return { reason, code: closeCode, fatal: false };
  }

  async run(signal) {
    let delay = 1000;
    try {
      while (!signal?.aborted) {
        try {
          await this.session(signal, () => { delay = 1000; console.log(`Device ${this.config.device_id} connected`); }, { preserveJobs: true });
        } catch (error) {
          if (error.fatal) throw error;
          if (!signal?.aborted) console.error(`Gateway disconnected; retrying in ${(delay / 1000).toFixed(1)}s`);
        }
        if (signal?.aborted) break;
        await pause(delay + Math.random() * 1000, signal); delay = Math.min(delay * 2, 30_000);
      }
    } finally { await this.close(); }
  }

  async close() {
    if (this.closeTask) return this.closeTask;
    this.closing = true;
    this.closeTask = this.finishClose();
    return this.closeTask;
  }

  async finishClose() {
    const cleanup = Promise.allSettled([this.processes.close(), this.searches.close(),
      ...this.inFlightOperations]);
    let timer;
    const deadline = new Promise((resolve) => { timer = setTimeout(resolve, this.shutdownTimeoutMs); });
    await Promise.race([cleanup, deadline]);
    clearTimeout(timer);
    this.cleanShutdown = this.inFlightOperations.size === 0 && this.searches.inFlight.size === 0 &&
      [...this.processes.jobs.values()].every((job) => job.closed);
    return { clean: this.cleanShutdown };
  }
}
