import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import WebSocketServer from '../vendor/ws/lib/websocket-server.js';
import { Agent } from '../lib/agent.mjs';

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
