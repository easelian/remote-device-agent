import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';
import { Agent } from '../.build/agent.mjs';

const token = 't'.repeat(48);

async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-agent-'));
  const server = new WebSocketServer({ port: 0, host: '127.0.0.1', verifyClient: ({ req }, done) => {
    done(req.headers.authorization === `Bearer ${token}`, 401, 'Unauthorized');
  }});
  await new Promise((resolve) => server.once('listening', resolve));
  const address = server.address();
  const config = { gateway_url: `http://127.0.0.1:${address.port}/`, device_id: 'test-device', token,
    roots: { work: root }, allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 30 };
  t.after(async () => {
    for (const socket of server.clients) socket.terminate();
    await closeServer(server);
    await fs.rm(root, { recursive: true, force: true });
  });
  return { root, server, config, agent: new Agent(config) };
}

function closeServer(server) {
  return new Promise((resolve) => server.close(() => resolve()));
}

function waitConnection(server) {
  return new Promise((resolve) => server.once('connection', (socket, request) => resolve({ socket, request })));
}

function messageQueue(socket) {
  const messages = [];
  const waiters = [];
  let closed;
  socket.on('message', (raw) => {
    const message = JSON.parse(raw.toString('utf8'));
    const waiter = waiters.shift();
    if (waiter) waiter.resolve(message); else messages.push(message);
  });
  socket.once('close', () => {
    closed = new Error('socket closed before message');
    while (waiters.length) waiters.shift().reject(closed);
  });
  return () => {
    if (messages.length) return Promise.resolve(messages.shift());
    if (closed) return Promise.reject(closed);
    return new Promise((resolve, reject) => waiters.push({ resolve, reject }));
  };
}

test('performs authenticated version-one handshake and correlates results', { timeout: 8000 }, async (t) => {
  const { server, config, agent } = await fixture(t);
  const peer = waitConnection(server);
  const session = agent.session(new AbortController().signal);
  const { socket, request } = await peer;
  const next = messageQueue(socket);
  assert.equal(request.headers.authorization, `Bearer ${token}`);
  const hello = await next();
  assert.equal(hello.type, 'hello');
  assert.equal(hello.version, 1);
  socket.send(JSON.stringify({ type: 'ready', version: 1, device_id: config.device_id }));
  socket.send(JSON.stringify({ type: 'request', id: 'info', operation: 'device_info' }));
  socket.send(JSON.stringify({ type: 'request', id: 'list', operation: 'list_directory', params: { root: 'work' } }));
  socket.send(JSON.stringify({ type: 'request', id: 'bad', operation: 'unknown_operation', params: {} }));
  const results = new Map();
  for (let i = 0; i < 3; i++) { const result = await next(); results.set(result.id, result); }
  assert.equal(results.get('info').result.roots.work, agent.files.roots.work);
  assert.ok(Array.isArray(results.get('list').result.entries));
  assert.match(results.get('bad').error, /Unknown operation/);
  socket.close();
  await session;
});

test('returns operation errors with request ids and accepts omitted params', { timeout: 8000 }, async (t) => {
  const { server, agent } = await fixture(t);
  const peer = waitConnection(server);
  const session = agent.session(new AbortController().signal);
  const { socket } = await peer;
  const next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'ready' }));
  socket.send(JSON.stringify({ type: 'request', id: 'missing', operation: 'read_file' }));
  const result = await next();
  assert.equal(result.type, 'result');
  assert.equal(result.id, 'missing');
  assert.match(result.error, /Unknown root|configured roots/);
  socket.close();
  await session;
});

test('rejects an invalid handshake message', { timeout: 8000 }, async (t) => {
  const { server, agent } = await fixture(t);
  const peer = waitConnection(server);
  const session = agent.session(new AbortController().signal);
  const { socket } = await peer;
  const next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'not-ready', version: 1 }));
  await assert.rejects(session, /handshake failed|connection failed/);
});

test('abort closes the session and terminates a running process', { timeout: 8000 }, async (t) => {
  const { server, agent } = await fixture(t);
  const peer = waitConnection(server);
  const controller = new AbortController();
  const session = agent.session(controller.signal);
  const { socket } = await peer;
  const next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'ready' }));
  socket.send(JSON.stringify({ type: 'request', id: 'start', operation: 'start_process', params: {
    command: 'sleep 30', root: 'work', cwd: '.', timeout_seconds: 30, wait_seconds: 0,
  }}));
  const started = await next();
  assert.equal(started.id, 'start');
  assert.equal(started.result.status, 'running');
  const pid = started.result.pid;
  controller.abort();
  await session;
  for (let i = 0; i < 50; i++) {
    try { process.kill(pid, 0); } catch (error) { if (error.code === 'ESRCH') break; throw error; }
    await new Promise((resolve) => setTimeout(resolve, 10));
    if (i === 49) assert.fail(`process ${pid} is still running`);
  }
});

test('run preserves a managed job and output across a transient disconnect', { timeout: 12000 }, async (t) => {
  const { server, config, agent } = await fixture(t);
  let connections = 0;
  let firstJob;
  let reconnected;
  const reconnectedResult = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Agent did not reconnect')), 9000);
    server.on('connection', (socket) => {
      connections++;
      const next = messageQueue(socket);
      void (async () => {
        const hello = await next();
        assert.equal(hello.type, 'hello');
        socket.send(JSON.stringify({ type: 'ready', version: 1 }));
        if (connections === 1) {
          socket.send(JSON.stringify({ type: 'request', id: 'start', operation: 'start_process', params: {
            command: "printf 'before'; sleep 30", root: 'work', cwd: '.', timeout_seconds: 30, wait_seconds: 0,
          }}));
          const started = await next();
          firstJob = started.result;
          setTimeout(() => socket.terminate(), 50);
        } else {
          socket.send(JSON.stringify({ type: 'request', id: 'read', operation: 'read_process', params: {
            job_id: firstJob.job_id, offset: 0, limit: 100,
          }}));
          const read = await next();
          clearTimeout(timer);
          reconnected = read.result;
          resolve(read.result);
        }
      })().catch(reject);
    });
  });
  const controller = new AbortController();
  const running = agent.run(controller.signal);
  const result = await reconnectedResult;
  assert.equal(connections, 2);
  assert.equal(result.status, 'running');
  assert.match(result.output, /before/);
  const pid = firstJob.pid;
  controller.abort();
  await running;
  for (let i = 0; i < 50; i++) {
    try { process.kill(pid, 0); } catch (error) { if (error.code === 'ESRCH') return; throw error; }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.fail(`process ${pid} is still running`);
});

test('close 1008 stops reconnect and cleans managed jobs', { timeout: 8000 }, async (t) => {
  const { server, config, agent } = await fixture(t);
  let connections = 0;
  let pid;
  const ready = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Agent did not connect')), 3000);
    server.once('connection', (socket) => {
      connections++;
      const next = messageQueue(socket);
      void (async () => {
        await next(); socket.send(JSON.stringify({ type: 'ready', version: 1 }));
        socket.send(JSON.stringify({ type: 'request', id: 'start', operation: 'start_process', params: {
          command: 'sleep 30', root: 'work', cwd: '.', timeout_seconds: 30, wait_seconds: 0,
        }}));
        pid = (await next()).result.pid;
        clearTimeout(timer); socket.close(1008, 'revoked'); resolve();
      })().catch(reject);
    });
  });
  const running = agent.run(new AbortController().signal);
  await ready;
  await assert.rejects(running, /authorization|revoked/);
  assert.equal(connections, 1);
  for (let i = 0; i < 50; i++) {
    try { process.kill(pid, 0); } catch (error) { if (error.code === 'ESRCH') return; throw error; }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.fail(`process ${pid} is still running`);
});

test('HTTP 401 handshake rejection stops reconnect', { timeout: 5000 }, async (t) => {
  const rejected = new WebSocketServer({ port: 0, host: '127.0.0.1', verifyClient: (_info, done) => done(false, 401, 'Unauthorized') });
  await new Promise((resolve) => rejected.once('listening', resolve));
  const address = rejected.address();
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-agent-401-'));
  const agent = new Agent({ gateway_url: `http://127.0.0.1:${address.port}/`, device_id: 'test-device', token, roots: { work: root },
    allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 30 });
  t.after(async () => { await closeServer(rejected); await fs.rm(root, { recursive: true, force: true }); });
  const started = Date.now();
  await assert.rejects(agent.run(new AbortController().signal), /authorization|revoked/);
  assert.ok(Date.now() - started < 2000);
});

test('HTTP 502 retries and a later accepted connection can be aborted', { timeout: 6000 }, async (t) => {
  let attempts = 0;
  const server = new WebSocketServer({ port: 0, host: '127.0.0.1', verifyClient: (_info, done) => {
    attempts++;
    if (attempts === 1) done(false, 502, 'Bad Gateway'); else done(true);
  }});
  await new Promise((resolve) => server.once('listening', resolve));
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-agent-502-'));
  const agent = new Agent({ gateway_url: `http://127.0.0.1:${server.address().port}/`, device_id: 'test-device', token,
    roots: { work: root }, allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 30 });
  const connected = new Promise((resolve) => server.once('connection', resolve));
  const controller = new AbortController();
  t.after(async () => { controller.abort(); await closeServer(server); await fs.rm(root, { recursive: true, force: true }); });
  const running = agent.run(controller.signal);
  await connected;
  assert.ok(attempts >= 2);
  controller.abort();
  await running;
});

test('HTTP 403 handshake rejection stops reconnect', { timeout: 5000 }, async (t) => {
  const rejected = new WebSocketServer({ port: 0, host: '127.0.0.1', verifyClient: (_info, done) => done(false, 403, 'Forbidden') });
  await new Promise((resolve) => rejected.once('listening', resolve));
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-agent-403-'));
  const agent = new Agent({ gateway_url: `http://127.0.0.1:${rejected.address().port}/`, device_id: 'test-device', token,
    roots: { work: root }, allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 30 });
  t.after(async () => { await closeServer(rejected); await fs.rm(root, { recursive: true, force: true }); });
  const started = Date.now();
  await assert.rejects(agent.run(new AbortController().signal), /authorization|revoked/);
  assert.ok(Date.now() - started < 2000);
});

test('device_info advertises the file, process, and search operations', async (t) => {
  const { agent } = await fixture(t);
  const info = agent.info();
  for (const operation of ['file_info', 'read_file_chunk', 'read_multiple_files', 'move_file',
    'start_search', 'get_search_results', 'stop_search', 'list_searches']) {
    assert.ok(info.capabilities.operations.includes(operation), operation);
  }
  assert.equal((await agent.dispatch('file_info', { root: 'work', path: '.' })).type, 'directory');
  assert.equal(info.offline_process_grace_seconds, 300);
});

test('unresolved I/O keeps a global operation budget across reconnect and shutdown is bounded', { timeout: 5000 }, async (t) => {
  const { server, agent } = await fixture(t);
  agent.operationTimeoutMs = 20;
  agent.shutdownTimeoutMs = 30;
  let dispatched = 0;
  agent.dispatch = () => { dispatched += 1; return new Promise(() => {}); };
  let peer = waitConnection(server);
  const first = agent.session(new AbortController().signal, () => {}, { preserveJobs: true });
  let { socket } = await peer;
  let next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'ready' }));
  for (let i = 0; i < 8; i++) socket.send(JSON.stringify({ type: 'request', id: `hung-${i}`, operation: 'hung' }));
  for (let i = 0; i < 8; i++) assert.match((await next()).error, /deadline.*unknown/);
  assert.equal(agent.inFlightOperations.size, 8);
  socket.close();
  await first;

  peer = waitConnection(server);
  const stop = new AbortController();
  const second = agent.session(stop.signal, () => {}, { preserveJobs: true });
  ({ socket } = await peer);
  next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'ready' }));
  socket.send(JSON.stringify({ type: 'request', id: 'fresh', operation: 'hung' }));
  assert.match((await next()).error, /Device busy/);
  assert.equal(dispatched, 8);
  const started = Date.now();
  stop.abort();
  await second;
  assert.ok(Date.now() - started < 500);
  assert.equal(agent.inFlightOperations.size, 8, 'I/O was not falsely reported as cancelled');
});

test('duplicate in-flight request IDs cannot execute a write twice', { timeout: 5000 }, async (t) => {
  const { server, agent } = await fixture(t);
  agent.shutdownTimeoutMs = 30;
  let dispatched = 0;
  agent.dispatch = () => { dispatched += 1; return new Promise(() => {}); };
  const peer = waitConnection(server);
  const session = agent.session(new AbortController().signal);
  const { socket } = await peer;
  const next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'ready' }));
  const request = JSON.stringify({ type: 'request', id: 'same', operation: 'write_file' });
  socket.send(request);
  socket.send(request);
  await session;
  assert.equal(dispatched, 1);
});

test('shutdown rejects new requests before dispatch', { timeout: 5000 }, async (t) => {
  const { server, agent } = await fixture(t);
  const peer = waitConnection(server);
  const session = agent.session(new AbortController().signal);
  const { socket } = await peer;
  const next = messageQueue(socket);
  await next();
  socket.send(JSON.stringify({ type: 'ready' }));
  // Ensure the ready frame has been consumed before beginning shutdown.
  socket.send(JSON.stringify({ type: 'request', id: 'initial', operation: 'device_info' }));
  assert.equal((await next()).id, 'initial');
  await agent.close();
  let dispatched = 0;
  agent.dispatch = () => { dispatched += 1; return {}; };
  socket.send(JSON.stringify({ type: 'request', id: 'late', operation: 'write_file' }));
  assert.match((await next()).error, /shutting down/);
  assert.equal(dispatched, 0);
  assert.equal(agent.inFlightOperations.size, 0);
  socket.close();
  await session;
});
