import os from 'node:os';
import WebSocket from '../vendor/ws/lib/websocket.js';
import { Files } from './files.mjs';
import { Processes } from './processes.mjs';

const pause = (milliseconds, signal) => new Promise((resolve) => {
  if (signal?.aborted) return resolve();
  const done = () => { clearTimeout(timer); signal?.removeEventListener('abort', done); resolve(); };
  const timer = setTimeout(done, milliseconds);
  signal?.addEventListener('abort', done, { once: true });
});

export class Agent {
  constructor(config) {
    this.config = config;
    this.files = new Files(config.roots, config.allow_write);
    this.processes = new Processes(config, this.files);
  }

  info() {
    return { os: process.platform === 'darwin' ? 'Darwin' : 'Linux', release: os.release(), machine: os.machine(),
      hostname: os.hostname(), roots: this.files.roots, allow_shell: this.config.allow_shell,
      allow_write: this.config.allow_write, max_processes: this.config.max_processes,
      max_process_seconds: this.config.max_process_seconds, shell: this.config.shell,
      file_limit_bytes: 1024 * 1024, process_output_limit_chars: 256 * 1024,
      shell_authority: 'Agent OS user; file roots do not sandbox shell commands' };
  }

  async dispatch(operation, params) {
    const files = { list_directory: 'list', read_file: 'read', write_file: 'write', edit_file: 'edit',
      make_directory: 'mkdir', search_files: 'search' };
    if (operation === 'device_info') return this.info();
    if (Object.hasOwn(files, operation)) return this.files[files[operation]](params);
    const p = this.processes;
    switch (operation) {
      case 'start_process': return p.start(params.command, params.root, params.cwd, params.timeout_seconds, params.wait_seconds);
      case 'read_process': return p.read(params.job_id, params.offset, params.limit);
      case 'list_processes': return p.list();
      case 'write_process_stdin': return p.writeStdin(params.job_id, params.text, params.close);
      case 'terminate_process': return p.terminate(params.job_id);
      default: throw new Error('Unknown operation');
    }
  }

  async session(signal, onReady = () => {}) {
    const url = new URL(this.config.gateway_url);
    url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
    url.pathname = '/agent/' + this.config.device_id;
    const ws = new WebSocket(url, { headers: { Authorization: 'Bearer ' + this.config.token },
      maxPayload: 2 * 1024 * 1024, handshakeTimeout: 15_000, perMessageDeflate: false, followRedirects: false });
    const pending = new Set();
    let heartbeat; let readyTimeout; let alive = true; let ready = false;
    const stop = () => ws.terminate();
    const send = (value) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(value), (error) => { if (error) stop(); });
    };
    if (signal?.aborted) stop();
    signal?.addEventListener('abort', stop, { once: true });
    try {
      await new Promise((resolve, reject) => {
        ws.once('close', () => { ready ? resolve() : reject(new Error('Gateway handshake failed')); });
        ws.on('error', () => { if (!ready) reject(new Error('Gateway connection failed')); });
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
            clearTimeout(readyTimeout); ready = true; onReady();
            heartbeat = setInterval(() => {
              if (!alive) { stop(); return; }
              alive = false; ws.ping(); send({ type: 'heartbeat' });
            }, 20_000);
            return;
          }
          if (message.type === 'heartbeat') return;
          const params = message.params ?? {};
          if (message.type !== 'request' || typeof message.id !== 'string' ||
              typeof params !== 'object' || Array.isArray(params)) { stop(); return; }
          if (pending.size >= 8) { send({ type: 'result', id: message.id, error: 'Device busy' }); return; }
          const task = this.dispatch(message.operation, params).then(
            (result) => send({ type: 'result', id: message.id, result }),
            (error) => send({ type: 'result', id: message.id, error: String(error.message).slice(0, 1000) }));
          pending.add(task); task.finally(() => pending.delete(task));
        });
      });
    } finally {
      clearTimeout(readyTimeout); clearInterval(heartbeat); signal?.removeEventListener('abort', stop);
      ws.terminate(); await Promise.allSettled(pending); await this.processes.close();
    }
  }

  async run(signal) {
    let delay = 1000;
    try {
      while (!signal?.aborted) {
        try { await this.session(signal, () => { delay = 1000; console.log(`Device ${this.config.device_id} connected`); }); }
        catch { if (!signal?.aborted) console.error(`Gateway disconnected; retrying in ${(delay / 1000).toFixed(1)}s`); }
        await pause(delay + Math.random() * 1000, signal); delay = Math.min(delay * 2, 30_000);
      }
    } finally { await this.processes.close(); }
  }
}
