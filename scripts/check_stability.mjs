#!/usr/bin/env node

// Reproducible local stability probe. It owns a temporary root and an
// in-process real WebSocket gateway; it never mutates a production profile.
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { monitorEventLoopDelay } from 'node:perf_hooks';
import { Agent } from '../lib/agent.mjs';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';

const MB = 1024 * 1024;
const MAX_SAMPLES = 2048;
const argv = process.argv.slice(2);
const has = (name) => argv.includes(`--${name}`);
const value = (name, fallback) => { const i = argv.indexOf(`--${name}`); return i < 0 ? fallback : argv[i + 1]; };
const allowedOptions = new Set(['--json', '--quick', '--duration-seconds']);
for (let index = 0; index < argv.length; index += 1) {
  const arg = argv[index];
  if (!arg.startsWith('--') || !allowedOptions.has(arg)) throw new Error(`unknown option or argument: ${arg}`);
  if (arg === '--duration-seconds' && (index + 1 >= argv.length || argv[index + 1].startsWith('--'))) throw new Error('--duration-seconds requires a value');
  if (arg === '--duration-seconds') index += 1;
}
if (has('server') || has('device-id') || has('token')) throw new Error('This probe is local-fixture only; server/device/token are unsupported');
const durationSeconds = Number(value('duration-seconds', has('quick') ? 120 : 86400));
if (!Number.isFinite(durationSeconds) || durationSeconds < 1 || durationSeconds > 7 * 86400) throw new Error('invalid duration');
const json = has('json');
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const closeServer = (server) => new Promise((resolve) => server.close(resolve));
const percentile = (values, p) => { if (!values.length) return 0; const sorted = [...values].sort((a, b) => a - b); return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * p) - 1)]; };
async function fdCount() { try { return (await fs.readdir(process.platform === 'linux' ? '/proc/self/fd' : '/dev/fd')).length; } catch { return null; } }
function alive(pids) { return pids.filter((pid) => { try { process.kill(pid, 0); return true; } catch (e) { return e.code === 'EPERM'; } }).length; }

function boundedPeer(socket, state) {
  const pending = new Map(); let closed = false; const maxPending = 32;
  const onMessage = (raw) => { let msg; try { msg = JSON.parse(raw.toString('utf8')); } catch { state.errors.push('invalid gateway message'); return; } if (msg.type === 'result' && typeof msg.id === 'string') { const wait = pending.get(msg.id); if (wait) { pending.delete(msg.id); wait(msg); } } };
  socket.on('message', onMessage);
  socket.once('close', () => { closed = true; for (const [, wait] of pending) wait({ error: 'socket closed' }); pending.clear(); });
  const rpc = (operation, params = {}, timeout = 10000) => new Promise((resolve) => { if (closed || socket.readyState !== 1) return resolve({ error: 'socket unavailable' }); if (pending.size >= maxPending) return resolve({ error: 'peer pending budget exceeded' }); const id = `probe-${++state.requestId}`; state.peerPendingMax = Math.max(state.peerPendingMax || 0, pending.size + 1); const timer = setTimeout(() => { pending.delete(id); resolve({ error: `RPC timeout: ${operation}` }); }, timeout); pending.set(id, (result) => { clearTimeout(timer); resolve(result); }); socket.send(JSON.stringify({ type: 'request', id, operation, params })); });
  let resolveClosed;
  const closedPromise = new Promise((resolve) => { resolveClosed = resolve; });
  socket.once('close', () => resolveClosed());
  return {
    socket,
    rpc,
    ready: false,
    closedPromise,
    pendingCount: () => pending.size,
  };
}

async function run() {
  const originalLog = console.log;
  if (json) console.log = (...items) => { if (!String(items[0] ?? '').startsWith('Device ')) originalLog(...items); };
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-stability-'));
  const token = randomBytes(32).toString('base64url'); const deviceId = `stability-${process.pid}`;
  const state = { connections: 0, requestId: 0, errors: [], flapErrors: [], errorCount: 0, peers: new Set(), active: null, jobId: null, pid: null, preserved: false, flapCount: 0, peerPendingMax: 0 };
  const server = new WebSocketServer({ port: 0, host: '127.0.0.1' }); await new Promise((resolve) => server.once('listening', resolve));
  server.on('connection', (socket, request) => { if (request.headers.authorization !== `Bearer ${token}`) return socket.terminate(); state.connections += 1; const peer = boundedPeer(socket, state); state.peers.add(peer); state.active = peer; socket.once('close', () => state.peers.delete(peer)); socket.once('message', (raw) => { let hello; try { hello = JSON.parse(raw.toString('utf8')); } catch { return socket.terminate(); } if (hello.type !== 'hello' || hello.version !== 1) return socket.terminate(); peer.ready = true; socket.send(JSON.stringify({ type: 'ready', version: 1, device_id: deviceId })); }); });
  const config = { gateway_url: `http://127.0.0.1:${server.address().port}/`, device_id: deviceId, token, roots: { work: root }, allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 3600 };
  const agent = new Agent(config, { operationTimeoutMs: 10000, shutdownTimeoutMs: 8000 });
  const baselineFd = await fdCount(); const baselineHandles = process._getActiveHandles().length;
  const metrics = { ok: false, duration_seconds: durationSeconds, loops: 0, operations: 0, successful_operations: 0, network_skips: 0, rpc_errors: 0, search_ops: 0, errors: [], event_loop_lag_p99_ms: 0, event_loop_lag_max_ms: 0, rss_max_bytes: 0, fd_max: 0, active_handles_max: 0, jobs_max: 0, searches_max: 0, processes_max: 0, stdout_benchmark_bytes: 100 * MB, stdout_benchmark_completed: false, stdout_ring_bytes: 0, reconnect: null, cleanup: null };
  const histogram = monitorEventLoopDelay({ resolution: 20 }); histogram.enable(); const lagSamples = []; let sampling = false; const sampleTasks = new Set();
  const sampleOne = async () => { if (sampling) return; sampling = true; try { const jobs = agent.processes.list().jobs; const lag = histogram.percentile(99) / 1e6; metrics.event_loop_lag_p99_ms = lag; metrics.event_loop_lag_max_ms = histogram.max / 1e6; if (lagSamples.length < MAX_SAMPLES) lagSamples.push(lag); metrics.rss_max_bytes = Math.max(metrics.rss_max_bytes, process.memoryUsage().rss); const fd = await fdCount(); if (fd !== null) metrics.fd_max = Math.max(metrics.fd_max, fd); metrics.active_handles_max = Math.max(metrics.active_handles_max, process._getActiveHandles().length); metrics.jobs_max = Math.max(metrics.jobs_max, jobs.filter((j) => j.status === 'running').length); metrics.processes_max = Math.max(metrics.processes_max, alive(jobs.map((j) => j.pid).filter(Boolean))); metrics.searches_max = Math.max(metrics.searches_max, agent.searches.inFlight.size); } finally { sampling = false; } };
  const sample = setInterval(() => { const task = sampleOne().catch((error) => recordError(`sample: ${error.message}`)); sampleTasks.add(task); task.finally(() => sampleTasks.delete(task)); }, 1000);
  const controller = new AbortController(); let running; const deadline = Date.now() + durationSeconds * 1000;
  const recordError = (message, flap = false) => { state.errorCount += 1; const target = flap ? state.flapErrors : state.errors; if (target.length < 64) target.push(message); };
  const rpc = async (operation, params = {}, timeout = 10000) => { metrics.operations += 1; const waitUntil = Date.now() + timeout; while ((!state.active || !state.active.ready || state.active.socket.readyState !== 1) && Date.now() < waitUntil) await sleep(20); const peer = state.active; if (!peer || !peer.ready || peer.socket.readyState !== 1) { metrics.network_skips += 1; return null; } const result = await peer.rpc(operation, params, timeout); if (result.error) { if (result.error !== 'socket closed') { metrics.rpc_errors += 1; recordError(`${operation}: ${result.error}`); } return null; } metrics.successful_operations += 1; return result.result ?? null; };
  let flapTimer = null; const flapTasks = new Set(); let flapBusy = false;
  const flapStep = async () => {
    if (flapBusy || Date.now() >= deadline || !state.active || state.active.socket.readyState !== 1) return;
    flapBusy = true;
    try {
      state.flapCount += 1; state.active.socket.terminate(); const limit = Math.min(deadline, Date.now() + 10000);
      while ((!state.active || !state.active.ready || state.active.socket.readyState !== 1) && Date.now() < limit) await sleep(20);
      if (!state.active || !state.active.ready || state.active.socket.readyState !== 1) { recordError('reconnect timeout after scheduled flap', true); return; }
      const read = await rpc('read_process', { job_id: state.jobId, offset: 0, limit: 64 });
      if (read?.pid !== state.pid || read.status !== 'running') recordError('scheduled flap did not preserve running PID', true);
    } finally { flapBusy = false; }
  };
  try {
    for (let i = 0; i < 256; i += 1) { const d = path.join(root, `tree-${i % 16}`); await fs.mkdir(d, { recursive: true }); await fs.writeFile(path.join(d, `entry-${i}.txt`), `needle ${i}\n`); }
    running = agent.run(controller.signal).catch((e) => recordError(`agent: ${e.message}`));
    while (!state.active || !state.active.ready || state.active.socket.readyState !== 1) { if (Date.now() >= deadline) throw new Error('initial connection timeout'); await sleep(20); }
    const first = await rpc('start_process', { command: "while :; do printf 'stability\\n'; sleep 0.05; done", root: 'work', timeout_seconds: 3600, wait_seconds: 0.25 }); state.jobId = first?.job_id; state.pid = first?.pid; if (!state.jobId || !state.pid) recordError('initial process did not return job_id/pid'); state.active.socket.terminate(); state.flapCount += 1;
    while (!state.preserved && Date.now() < deadline) { await sleep(20); if (state.connections > 1 && state.active?.ready && state.active.socket.readyState === 1) { const read = await rpc('read_process', { job_id: state.jobId, offset: 0, limit: 64 }); if (read?.pid === state.pid && read.status === 'running') state.preserved = true; } }
    const flapInterval = Math.max(1000, Math.min(30000, Math.floor(durationSeconds * 1000 / 3))); flapTimer = setInterval(() => { const task = flapStep().catch((error) => recordError(`flap: ${error.message}`, true)); flapTasks.add(task); task.finally(() => flapTasks.delete(task)); }, flapInterval);
    while (Date.now() < deadline) {
      metrics.loops += 1; await rpc('list_directory', { root: 'work' }); await rpc('read_file', { root: 'work', path: `tree-${metrics.loops % 16}/entry-${metrics.loops % 256}.txt` }); await rpc('read_file_chunk', { root: 'work', path: 'tree-0/entry-0.txt', offset: 0, length: 64 }); await rpc('read_multiple_files', { files: [{ root: 'work', path: 'tree-0/entry-0.txt' }, { root: 'work', path: 'tree-1/entry-1.txt' }] }); await sleep(100);
      const search = await rpc('start_search', { root: 'work', query: 'needle', content: true }); if (search?.search_id) { metrics.search_ops += 1; let status = 'queued'; for (let i = 0; i < 20 && ['queued', 'running'].includes(status); i += 1) { await sleep(10); const page = await rpc('get_search_results', { search_id: search.search_id, limit: 200 }); status = page?.status; } await rpc('stop_search', { search_id: search.search_id }); }
      if (metrics.loops % 4 === 0) { const read = await rpc('read_process', { job_id: state.jobId, offset: 0, limit: 64 }); if (read && (read.pid !== state.pid || read.status !== 'running')) recordError('job PID changed during soak'); }
      if (metrics.loops > 1 && Date.now() + 15000 >= deadline) break;
    }
    const benchmarkStarted = Date.now();
    if (Date.now() < deadline) {
      const stdout = await rpc('start_process', { command: `head -c ${100 * MB} /dev/zero | tr '\\0' x`, root: 'work', timeout_seconds: 120, wait_seconds: 0.25 }, Math.min(15000, Math.max(1, deadline - Date.now())));
      if (stdout?.job_id) { let final = stdout; const end = Math.min(deadline, Date.now() + 120000); while (final?.status === 'running' && Date.now() < end) { await sleep(Math.min(100, Math.max(1, end - Date.now()))); final = await rpc('read_process', { job_id: stdout.job_id, offset: 0, limit: 65536 }, Math.min(15000, Math.max(1, end - Date.now()))); } const job = agent.processes.jobs.get(stdout.job_id); metrics.stdout_benchmark_completed = final?.status === 'exited' && final?.exit_code === 0; metrics.stdout_ring_bytes = job?.output_length ?? 0; metrics.stdout_segments = job?.segments.length ?? 0; metrics.stdout_base_offset = job?.base_offset ?? 0; metrics.stdout_output_dropped = Boolean(final?.output_dropped); metrics.stdout_captured_bytes = metrics.stdout_base_offset + metrics.stdout_ring_bytes; if (metrics.stdout_ring_bytes > 256 * 1024) recordError('stdout ring exceeded 256 KiB'); }
    }
    metrics.stdout_benchmark_elapsed_ms = Date.now() - benchmarkStarted;
    clearInterval(flapTimer); flapTimer = null; await Promise.allSettled([...flapTasks]);
  } catch (e) { recordError(e.message); }
  if (flapTimer) clearInterval(flapTimer);
  await Promise.allSettled([...flapTasks]);
  clearInterval(sample);
  await Promise.allSettled([...sampleTasks]);
  controller.abort();
  await running?.catch(() => {});
  await agent.close();

  // Agent.close() must finish before the fixture peers are torn down. Waiting
  // for the peer close events makes the cleanup counters observe real state,
  // rather than the state immediately after terminate() was requested.
  const peersAtCleanup = [...state.peers];
  for (const peer of peersAtCleanup) peer.socket.terminate();
  await Promise.allSettled(peersAtCleanup.map((peer) => peer.closedPromise));
  await closeServer(server);

  const jobs = agent.processes.list().jobs;
  const cleanupFd = await fdCount();
  const pendingRpc = [...peersAtCleanup]
    .reduce((total, peer) => total + peer.pendingCount(), 0);
  metrics.cleanup = {
    clean_shutdown: agent.cleanShutdown === true,
    peers_remaining: state.peers.size,
    pending_rpc: pendingRpc,
    jobs_running: jobs.filter((j) => j.status === 'running').length,
    in_flight_operations: agent.inFlightOperations.size,
    search_in_flight: agent.searches.inFlight.size,
    live_processes: alive(jobs.map((j) => j.pid).filter(Boolean)),
    fd: cleanupFd,
    fd_delta: baselineFd === null || cleanupFd === null ? null : cleanupFd - baselineFd,
    // Diagnostic only: Node retains unrelated handles during normal teardown.
    active_handles: process._getActiveHandles().length,
    active_handles_delta: process._getActiveHandles().length - baselineHandles,
  };
  metrics.reconnect = {
    connections: state.connections,
    flaps: state.flapCount,
    preserved_pid: state.preserved,
    flap_errors: state.flapErrors,
  };
  metrics.peer_pending_max = state.peerPendingMax;
  metrics.errors.push(...state.errors, ...state.flapErrors);
  metrics.error_count = state.errorCount;
  metrics.sample_p99_ms = percentile(lagSamples, 0.99);
  histogram.disable();
  await fs.rm(root, { recursive: true, force: true });

  metrics.ok = metrics.error_count === 0 &&
    metrics.loops > 1 &&
    metrics.successful_operations > 0 &&
    metrics.search_ops > 0 &&
    metrics.reconnect.flaps >= 2 &&
    metrics.reconnect.preserved_pid &&
    metrics.stdout_benchmark_completed &&
    metrics.stdout_ring_bytes === 256 * 1024 &&
    metrics.stdout_base_offset > 0 &&
    metrics.stdout_output_dropped &&
    metrics.stdout_captured_bytes >= 100 * MB &&
    metrics.cleanup.clean_shutdown &&
    metrics.cleanup.peers_remaining === 0 &&
    metrics.cleanup.pending_rpc === 0 &&
    metrics.cleanup.jobs_running === 0 &&
    metrics.cleanup.in_flight_operations === 0 &&
    metrics.cleanup.search_in_flight === 0 &&
    metrics.cleanup.live_processes === 0 &&
    (metrics.cleanup.fd_delta === null || metrics.cleanup.fd_delta <= 2);
  const thresholds = {
    errors: 0,
    loops_min: 2,
    successful_operations_min: 1,
    search_ops_min: 1,
    flaps_min: 2,
    stdout_ring_bytes: 256 * 1024,
    stdout_captured_bytes: 100 * MB,
    cleanup_clean_shutdown: true,
    cleanup_peers_remaining: 0,
    cleanup_pending_rpc: 0,
    cleanup_live_processes: 0,
  };
  const result = { ok: metrics.ok, thresholds, metrics };
  if (json) console.log(JSON.stringify(result));
  else console.log(JSON.stringify(result, null, 2));
  if (!result.ok) process.exitCode = 1;
  console.log = originalLog;
}
run().catch((e) => { console.error(e.stack || e.message); process.exitCode = 1; });
