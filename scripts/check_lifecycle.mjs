#!/usr/bin/env node

// Real-clock lifecycle proof for the device Agent.  This intentionally uses a
// loopback WebSocket server and the public Agent.session/start_process RPCs.
// It does not fake time or alter the Agent grace timer.
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { createHash, randomBytes } from 'node:crypto';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';
import { Agent } from '../lib/agent.mjs';

const NODE_VERSION = process.versions.node;
const TOKEN = 'lifecycle-' + randomBytes(24).toString('hex');
const GRACE_SECONDS = 300;
const MAX_RPC_MS = 20_000;
const CLEANUP_MS = 6_000;
const QUICK_INTERVALS = [1];
const FULL_INTERVALS = [1, 10, 60, 299, 301];

const sleep = (ms, signal) => new Promise((resolve) => {
  let timer;
  const done = () => { clearTimeout(timer); signal?.removeEventListener('abort', done); resolve(); };
  timer = setTimeout(done, ms);
  signal?.addEventListener('abort', done, { once: true });
});
const now = () => new Date().toISOString();
const elapsed = (from, to) => Number(((to - from) / 1000).toFixed(3));
const progress = (event, fields = {}) => process.stderr.write(`${JSON.stringify({ event, at: now(), pid: process.pid, ...fields })}\n`);
const activeControllers = new Set();
const activeFixtures = new Set();
const RUNTIME_FILES = ['lib/agent.mjs', 'lib/processes.mjs', 'lib/searches.mjs', 'lib/transport.mjs', 'package.json', 'bin/remote-device-hub.mjs'];
async function runtimeFingerprint() {
  const hash = createHash('sha256');
  const files = {};
  for (const relative of RUNTIME_FILES) { const data = await fs.readFile(new URL(`../${relative}`, import.meta.url)); files[relative] = createHash('sha256').update(data).digest('hex'); hash.update(relative); hash.update(data); }
  return { files, sha256: hash.digest('hex') };
}
const withTimeout = async (promise, ms, label) => {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms); }),
    ]);
  } finally { clearTimeout(timer); }
};

function processState(pid) {
  if (!pid) return { running: false, errno: 'ESRCH' };
  const probe = (target) => {
    try { process.kill(target, 0); return { running: true }; }
    catch (error) { return { running: false, errno: error.code || String(error) }; }
  };
  const processProbe = probe(pid);
  const groupProbe = probe(-pid);
  return { running: processProbe.running, errno: processProbe.errno, process_group: groupProbe };
}

function closeServer(server) {
  return new Promise((resolve) => {
    let done = false;
    const fallback = setTimeout(finish, CLEANUP_MS);
    function finish() { if (!done) { done = true; clearTimeout(fallback); resolve(); } }
    server.close(finish);
  });
}

async function fdCount() {
  try { return (await fs.readdir('/dev/fd')).length; } catch { return null; }
}

class Fixture {
  constructor({ rejectHttp = false } = {}) {
    this.rejectHttp = rejectHttp;
    this.rejections = [];
    this.connections = [];
    this.waiters = [];
    this.pendingRpc = new Set();
    this.server = new WebSocketServer({
      port: 0,
      host: '127.0.0.1',
      verifyClient: ({ req }, done) => {
        if (this.rejectHttp) {
          this.rejections.push({ status: this.rejectHttp, at: now() });
          return done(false, this.rejectHttp, 'lifecycle fixture rejection');
        }
        done(req.headers.authorization === `Bearer ${TOKEN}`, 401, 'Unauthorized');
      },
    });
    this.ready = new Promise((resolve) => this.server.once('listening', resolve));
    this.server.on('connection', (socket) => {
      const connection = { socket, openedAt: Date.now(), hello: null, readyAt: null, closedAt: null, queue: [], waiters: [], closed: false };
      this.connections.push(connection);
      progress('fixture_connection', { connections: this.connections.length });
      socket.on('close', (code, reason) => {
        connection.closed = true;
        connection.closedAt = Date.now();
        connection.closeCode = code;
        connection.closeReason = reason?.toString();
        while (connection.waiters.length) connection.waiters.shift().reject(new Error('fixture socket closed'));
      });
      socket.on('message', (raw) => this.onMessage(connection, raw));
      this.waiters.splice(0).forEach((waiter) => waiter.resolve(connection));
    });
  }

  async listen() {
    await this.ready;
    const address = this.server.address();
    this.url = `http://127.0.0.1:${address.port}/`;
    return this;
  }

  onMessage(connection, raw) {
    let message;
    try { message = JSON.parse(raw.toString('utf8')); } catch { connection.socket.close(1002); return; }
    if (message.type === 'hello') {
      connection.hello = message;
      connection.readyAt = Date.now();
      connection.socket.send(JSON.stringify({ type: 'ready', version: 1, device_id: 'lifecycle-device' }));
      return;
    }
    if (message.type === 'heartbeat') {
      connection.socket.send(JSON.stringify({ type: 'heartbeat' }));
      return;
    }
    if (message.type === 'result' || !message.id) return;
    // The Agent is the server-side RPC client in this fixture.  Requests are
    // sent by rpc() below; responses are handled by rpc()'s message waiter.
  }

  nextConnection(after = 0) {
    const existing = this.connections.find((connection, index) => index >= after);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('fixture connection timed out')), MAX_RPC_MS);
      this.waiters.push({ resolve: (value) => { clearTimeout(timer); resolve(value); }, reject });
    });
  }

  async waitReady(connection) {
    if (connection.readyAt) return connection;
    return withTimeout(new Promise((resolve, reject) => {
      const onMessage = (raw) => {
        try { const message = JSON.parse(raw.toString('utf8')); if (message.type === 'hello') { connection.readyAt = Date.now(); cleanup(); resolve(connection); } } catch {}
      };
      const onClose = () => { cleanup(); reject(new Error('fixture closed before ready')); };
      const cleanup = () => { connection.socket.removeListener('message', onMessage); connection.socket.removeListener('close', onClose); };
      connection.socket.on('message', onMessage);
      connection.socket.once('close', onClose);
    }), MAX_RPC_MS, 'agent ready');
  }

  async rpc(connection, operation, params = {}) {
    await this.waitReady(connection);
    const id = `${operation}-${randomBytes(6).toString('hex')}`;
    const response = new Promise((resolve, reject) => {
      this.pendingRpc.add(id);
      const timer = setTimeout(() => reject(new Error(`RPC ${operation} timed out`)), MAX_RPC_MS);
      const onMessage = (raw) => {
        let message;
        try { message = JSON.parse(raw.toString('utf8')); } catch { return; }
        if (message.type === 'result' && message.id === id) {
          cleanup();
          if (message.error) reject(new Error(message.error)); else resolve(message.result);
        }
      };
      const onClose = () => { cleanup(); reject(new Error(`RPC ${operation} socket closed`)); };
      const cleanup = () => { clearTimeout(timer); this.pendingRpc.delete(id); connection.socket.removeListener('message', onMessage); connection.socket.removeListener('close', onClose); };
      connection.socket.on('message', onMessage);
      connection.socket.once('close', onClose);
    });
    connection.socket.send(JSON.stringify({ type: 'request', id, operation, params }));
    return response;
  }

  markOffline(connection, code = 1001, reason = 'network path unavailable') {
    const markedAt = Date.now();
    connection.socket.close(code, reason);
    return markedAt;
  }

  async cleanup() {
    while (this.waiters.length) this.waiters.shift().reject(new Error('fixture cleanup'));
    for (const connection of this.connections) {
      try { connection.socket.terminate(); } catch {}
    }
    await closeServer(this.server);
  }
}

function config(root, fixture) {
  return {
    gateway_url: fixture.url,
    device_id: 'lifecycle-device',
    token: TOKEN,
    roots: { work: root },
    allow_shell: true,
    allow_write: true,
    shell: '/bin/sh',
    max_processes: 4,
    max_process_seconds: 3600,
  };
}

async function assertDead(pid, deadline = Date.now() + CLEANUP_MS) {
  let state = processState(pid);
  while (state.running && Date.now() < deadline) { await sleep(50); state = processState(pid); }
  return state;
}

async function cleanupEvidence(actualAgent, fixture, controller, sessions, root, pid, fdBefore) {
  controller.abort();
  await withTimeout(Promise.allSettled(sessions), CLEANUP_MS, 'session cleanup');
  await withTimeout(actualAgent.close(), CLEANUP_MS, 'agent cleanup');
  const closedState = await assertDead(pid);
  await withTimeout(fixture.cleanup(), CLEANUP_MS, 'fixture cleanup');
  const afterFd = await fdCount();
  await fs.rm(root, { recursive: true, force: true });
  const jobs = actualAgent.processes.jobs;
  const runningJobs = [...jobs.values()].filter((job) => job.status === 'running').length;
  const inFlight = actualAgent.inFlightOperations.size;
  const searchInFlight = actualAgent.searches.inFlight.size;
  const jobsClosed = [...jobs.values()].every((job) => job.closed);
  const activePeers = fixture.connections.filter((connection) => !connection.closed).length;
  const pendingRpc = fixture.pendingRpc.size;
  const tempRemoved = !(await fs.stat(root).then(() => true).catch(() => false));
  const fdDelta = fdBefore === null || afterFd === null ? null : afterFd - fdBefore;
  if (closedState.running || closedState.errno !== 'ESRCH' || closedState.process_group.running || closedState.process_group.errno !== 'ESRCH') throw new Error(`cleanup left PID/process group ${pid}: ${JSON.stringify(closedState)}`);
  if (!actualAgent.cleanShutdown || runningJobs !== 0 || !jobsClosed || inFlight !== 0 || searchInFlight !== 0 || activePeers !== 0 || pendingRpc !== 0 || !tempRemoved || (fdDelta !== null && fdDelta > 2)) {
    throw new Error(`cleanup invariant failed: ${JSON.stringify({ clean_shutdown: actualAgent.cleanShutdown, jobs: jobs.size, running_jobs: runningJobs, jobs_closed: jobsClosed, in_flight: inFlight, search_in_flight: searchInFlight, active_peers: activePeers, pending_rpc: pendingRpc, temp_removed: tempRemoved, fd_before: fdBefore, fd_after: afterFd, fd_delta: fdDelta })}`);
  }
  return { process: closedState, clean_shutdown: actualAgent.cleanShutdown, jobs: jobs.size, running_jobs: runningJobs, jobs_closed: jobsClosed, in_flight: inFlight, search_in_flight: searchInFlight, active_peers: activePeers, pending_rpc: pendingRpc, temp_removed: tempRemoved, fd_before: fdBefore, fd_after: afterFd, fd_delta: fdDelta };
}

async function runNetworkCase(intervalSeconds) {
  progress('case_start', { kind: 'network', interval_seconds: intervalSeconds });
  const startedAt = Date.now();
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-lifecycle-'));
  const fdBefore = await fdCount();
  const fixture = await new Fixture().listen();
  activeFixtures.add(fixture);
  const actualAgent = new Agent(config(root, fixture));
  const controller = new AbortController();
  activeControllers.add(controller);
  const sessions = [];
  let firstConnection;
  let pid;
  let job;
  let cleaned = false;
  const session = () => {
    const promise = actualAgent.session(controller.signal, () => {}, { preserveJobs: true });
    sessions.push(promise);
    return promise;
  };
  try {
    const firstIndex = fixture.connections.length;
    const firstSession = session();
    firstConnection = await fixture.nextConnection(firstIndex);
    await fixture.waitReady(firstConnection);
    job = await fixture.rpc(firstConnection, 'start_process', { command: 'sleep 3600', root: 'work', cwd: '.', timeout_seconds: 3600, wait_seconds: 0 });
    pid = job.pid;
    const beforeOffline = processState(pid);
    if (!beforeOffline.running) throw new Error(`job was not running after start_process: ${JSON.stringify(job)}`);
    const markOfflineAt = fixture.markOffline(firstConnection, 1001);
    const firstFinalAt = await withTimeout(firstSession.then(() => Date.now()), MAX_RPC_MS, 'first Agent session finalization');
    progress('session_final', { kind: 'network', interval_seconds: intervalSeconds, job_pid: pid, mark_offline_at: new Date(markOfflineAt).toISOString(), first_session_final_at: new Date(firstFinalAt).toISOString() });
    const offlineStartAt = firstFinalAt;
    const sleepStartAt = Date.now();
    await sleep(intervalSeconds * 1000, controller.signal);
    const reconnectStartedAt = Date.now();
    const nextIndex = fixture.connections.length;
    let reconnectPromise;
    let reconnectConnection;
    if (intervalSeconds < GRACE_SECONDS) {
      reconnectPromise = session();
      reconnectConnection = await fixture.nextConnection(nextIndex);
      await fixture.waitReady(reconnectConnection);
    } else {
      // Still exercise a real reconnect after the grace boundary, then prove
      // the preserved job was already killed while the agent was offline.
      reconnectPromise = session();
      reconnectConnection = await fixture.nextConnection(nextIndex);
      await fixture.waitReady(reconnectConnection);
    }
    const readyAt = reconnectConnection.readyAt || Date.now();
    progress('session_ready', { kind: 'network', interval_seconds: intervalSeconds, job_pid: pid, ready_at: new Date(readyAt).toISOString(), elapsed_seconds: elapsed(markOfflineAt, readyAt) });
    const readyElapsedFromMark = elapsed(markOfflineAt, readyAt);
    const readyElapsedFromFinal = elapsed(offlineStartAt, readyAt);
    const stateAtReady = processState(pid);
    let rpcProof = null;
    if (intervalSeconds < GRACE_SECONDS && readyElapsedFromFinal < GRACE_SECONDS) {
      const listed = await fixture.rpc(reconnectConnection, 'list_processes');
      const listedJob = listed.jobs.find((item) => item.pid === pid);
      if (!listedJob || listedJob.status !== 'running') throw new Error(`reconnected list_processes did not show PID ${pid} running`);
      const read = await fixture.rpc(reconnectConnection, 'read_process', { job_id: job.job_id, offset: 0, limit: 256 });
      if (read.pid !== pid || read.status !== 'running') throw new Error(`reconnected read_process did not show PID ${pid} running`);
      rpcProof = { list: listedJob, read: { pid: read.pid, status: read.status, job_id: read.job_id } };
    }
    const stateAfterBoundary = intervalSeconds >= GRACE_SECONDS ? await assertDead(pid) : stateAtReady;
    const passed = intervalSeconds < GRACE_SECONDS
      ? readyElapsedFromFinal < GRACE_SECONDS && stateAfterBoundary.running && rpcProof?.read?.pid === pid
      : !stateAfterBoundary.running && stateAfterBoundary.errno === 'ESRCH' && !stateAfterBoundary.process_group.running && stateAfterBoundary.process_group.errno === 'ESRCH';
    if (!passed) throw new Error(`lifecycle assertion failed at ${intervalSeconds}s: ${JSON.stringify({ readyElapsedFromMark, readyElapsedFromFinal, stateAtReady, stateAfterBoundary, rpcProof })}`);
    const cleanup = await cleanupEvidence(actualAgent, fixture, controller, sessions, root, pid, fdBefore);
    cleaned = true;
    return { kind: 'network', interval_seconds: intervalSeconds, passed, pid, job_id: job.job_id,
      timestamps: { started_at: new Date(startedAt).toISOString(), mark_offline_at: new Date(markOfflineAt).toISOString(),
        first_session_final_at: new Date(firstFinalAt).toISOString(), reconnect_started_at: new Date(reconnectStartedAt).toISOString(),
        ready_at: new Date(readyAt).toISOString(), sleep_started_at: new Date(sleepStartAt).toISOString() },
      elapsed_seconds: { mark_offline_to_ready: readyElapsedFromMark, first_session_final_to_ready: readyElapsedFromFinal },
      grace_seconds: GRACE_SECONDS, before_offline: beforeOffline, state_at_ready: stateAtReady,
      state_after_boundary: stateAfterBoundary, rpc_proof: rpcProof, cleanup,
      note: intervalSeconds >= GRACE_SECONDS ? 'ready after grace; no pre-grace availability claim' : 'ready before grace with same PID and RPC proof' };
  } catch (caught) {
    if (!cleaned) { await cleanupEvidence(actualAgent, fixture, controller, sessions, root, pid, fdBefore); cleaned = true; }
    throw caught;
  } finally {
    if (!cleaned && pid) await cleanupEvidence(actualAgent, fixture, controller, sessions, root, pid, fdBefore);
    activeControllers.delete(controller);
    activeFixtures.delete(fixture);
  }
}

async function runFatalCase(name, mode) {
  progress('case_start', { kind: 'fatal', name, mode });
  const startedAt = Date.now();
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-lifecycle-fatal-'));
  const fdBefore = await fdCount();
  const fixture = await new Fixture().listen();
  activeFixtures.add(fixture);
  const actualAgent = new Agent(config(root, fixture));
  const controller = new AbortController();
  activeControllers.add(controller);
  const sessions = [];
  let pid;
  let cleaned = false;
  try {
    const firstSession = actualAgent.session(controller.signal, () => {}, { preserveJobs: true });
    sessions.push(firstSession);
    const first = await fixture.nextConnection(0);
    await fixture.waitReady(first);
    const job = await fixture.rpc(first, 'start_process', { command: 'sleep 3600', root: 'work', cwd: '.', timeout_seconds: 3600, wait_seconds: 0 });
    pid = job.pid;
    const eventAt = Date.now();
    let final;
    let closeAt;
    let reconnectOutcome = null;
    if (mode === 'admin-revoke') {
      closeAt = fixture.markOffline(first, 1008, 'Device revoked by owner');
      const outcome = await withTimeout(firstSession.then(() => ({ status: 'fulfilled' }), (error) => ({ status: 'rejected', fatal: error.fatal, message: error.message })), MAX_RPC_MS, `${name} session failure`);
      final = [outcome];
      if (outcome.status !== 'rejected' || outcome.fatal !== true || first.closeCode !== 1008) throw new Error(`admin revoke proof failed: ${JSON.stringify({ outcome, close_code: first.closeCode })}`);
    } else {
      closeAt = fixture.markOffline(first, 1001, 'network path unavailable');
      await withTimeout(firstSession, MAX_RPC_MS, `${name} network session finalization`);
      const alive = processState(pid);
      if (!alive.running || !alive.process_group.running) throw new Error(`${name} job did not survive nonfatal network close: ${JSON.stringify(alive)}`);
      const expectedStatus = Number(mode.slice(-3));
      fixture.rejectHttp = expectedStatus;
      const reconnect = actualAgent.session(controller.signal, () => {}, { preserveJobs: true });
      sessions.push(reconnect);
      reconnectOutcome = await withTimeout(reconnect.then(() => ({ status: 'fulfilled' }), (error) => ({ status: 'rejected', fatal: error.fatal, message: error.message })), MAX_RPC_MS, `${name} authorization failure`);
      if (reconnectOutcome.status !== 'rejected' || reconnectOutcome.fatal !== true || !reconnectOutcome.message.includes(`HTTP ${expectedStatus}`)) {
        throw new Error(`${name} did not fail with HTTP ${expectedStatus}: ${JSON.stringify(reconnectOutcome)}`);
      }
      final = [{ status: 'fulfilled' }, reconnectOutcome];
    }
    const state = await assertDead(pid);
    progress('fatal_checked', { name, mode, job_pid: pid, state });
    if (state.running || state.errno !== 'ESRCH' || state.process_group.running || state.process_group.errno !== 'ESRCH') throw new Error(`${name} left PID/process group ${pid} alive: ${JSON.stringify(state)}`);
    if (mode !== 'admin-revoke' && !fixture.rejections.some((item) => item.status === Number(mode.slice(-3)))) {
      throw new Error(`${name} did not produce its expected HTTP rejection: ${JSON.stringify(fixture.rejections)}`);
    }
    const cleanup = await cleanupEvidence(actualAgent, fixture, controller, sessions, root, pid, fdBefore);
    cleaned = true;
    return { kind: 'fatal', name, mode, passed: true, pid, timestamps: { started_at: new Date(startedAt).toISOString(), fatal_event_at: new Date(eventAt).toISOString(), close_requested_at: new Date(closeAt).toISOString(), checked_at: now() }, state_after_failure: state, close_code: first.closeCode, http_rejections: fixture.rejections, authorization_outcome: reconnectOutcome, sessions: final.map((item) => item.status), cleanup };
  } finally {
    if (!cleaned && pid) await cleanupEvidence(actualAgent, fixture, controller, sessions, root, pid, fdBefore);
    activeControllers.delete(controller);
    activeFixtures.delete(fixture);
  }
}

async function main() {
  const args = new Set(process.argv.slice(2));
  const quick = args.has('--quick');
  const full = args.has('--full') || !quick;
  const outputIndex = process.argv.indexOf('--output');
  const outputPath = outputIndex >= 0 ? process.argv[outputIndex + 1] : null;
  if (outputIndex >= 0 && (!outputPath || outputPath.startsWith('--'))) throw new Error('--output requires a path');
  const intervals = full ? FULL_INTERVALS : QUICK_INTERVALS;
  const result = { schema: 1, script: 'check_lifecycle.mjs', node: NODE_VERSION, pid: process.pid, mode: full ? 'full' : 'quick', started_at: now(), grace_seconds: GRACE_SECONDS, runtime_source: await runtimeFingerprint(), cases: [], failures: [] };
  progress('suite_start', { mode: result.mode, intervals, grace_seconds: GRACE_SECONDS });
  const onSignal = () => {
    result.interrupted_at = now();
    for (const controller of activeControllers) controller.abort();
    for (const fixture of activeFixtures) void fixture.cleanup();
    process.exitCode = 130;
  };
  process.once('SIGINT', onSignal); process.once('SIGTERM', onSignal);
  for (const interval of intervals) {
    try { result.cases.push(await runNetworkCase(interval)); }
    catch (error) { result.failures.push({ case: `network-${interval}s`, error: String(error.stack || error) }); if (!quick) break; }
  }
  if (quick || full) {
    for (const [name, mode] of [['auth-http-401', 'auth-http-401'], ['auth-http-403', 'auth-http-403'], ['admin-revoke-1008', 'admin-revoke']]) {
      try { result.cases.push(await runFatalCase(name, mode)); }
      catch (error) { result.failures.push({ case: name, error: String(error.stack || error) }); }
    }
  }
  result.finished_at = now();
  result.passed = result.failures.length === 0 && result.cases.length === intervals.length + 3;
  const json = JSON.stringify(result, null, 2) + '\n';
  if (outputPath) await fs.writeFile(path.resolve(outputPath), json, { mode: 0o600 });
  process.stdout.write(json);
  process.exitCode = result.passed ? 0 : 1;
}

main().catch((error) => { process.stderr.write(`${error.stack || error}\n`); process.exitCode = 1; });
