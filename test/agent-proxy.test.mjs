import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import test from 'node:test';

const [major, minor] = process.versions.node.split('.').map(Number);
const supportsProxy = major > 24 || (major === 24 && minor >= 5) || (major === 22 && minor >= 21);
const proxyOptions = {
  skip: !supportsProxy && 'Node native proxy support requires 22.21+ or 24.5+',
};
const probe = `
  import { Agent } from './.build/agent.js';
  import { processEnvironment } from './.build/process-environment.js';
  import { globalAgent } from 'node:http';
  import { getDefaultAutoSelectFamilyAttemptTimeout } from 'node:net';
  import os from 'node:os';
  const overrides = [];
  const connect = globalAgent.createConnection;
  globalAgent.createConnection = function(options, callback) {
    overrides.push(options.autoSelectFamilyAttemptTimeout ?? null);
    return connect.call(this, options, callback);
  };
  const origin = process.env.RDH_TEST_PROXY_ORIGIN;
  const status = origin.startsWith('http:')
    ? (await fetch(origin + '/health', { signal: AbortSignal.timeout(4000) })).status
    : null;
  const agent = new Agent({ gateway_url: origin, device_id: 'proxy-test',
    token: 'device-token-' + 't'.repeat(48), roots: { work: os.tmpdir() },
    allow_shell: false, allow_write: false, shell: 'unused',
    max_processes: 1, max_process_seconds: 10 });
  let error = '';
  try { await agent.session(); } catch (failure) { error = failure.message; }
  await agent.close();
  const childEnv = processEnvironment();
  console.log(JSON.stringify({ status, error, overrides,
    timeout: getDefaultAutoSelectFamilyAttemptTimeout(),
    inheritedProxy: Object.keys(childEnv).filter(key => /proxy|node_options/i.test(key)) }));
`;

async function server(t, handler, upgrade) {
  const instance = http.createServer(handler);
  const sockets = new Set();
  instance.on('connection', (socket) => {
    sockets.add(socket);
    socket.on('close', () => sockets.delete(socket));
  });
  if (upgrade) {
    instance.on('upgrade', upgrade);
  }
  instance.listen(0, '127.0.0.1');
  await once(instance, 'listening');
  t.after(() => {
    for (const socket of sockets) {
      socket.destroy();
    }
    return new Promise((resolve) => instance.close(resolve));
  });
  return instance;
}

async function runProbe(origin, settings = {}, flags = []) {
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (/^(?:https?_proxy|all_proxy|no_proxy|node_use_env_proxy|node_options)$/i.test(key)) {
      delete env[key];
    }
  }
  Object.assign(env, settings, { RDH_TEST_PROXY_ORIGIN: origin });
  const child = spawn(process.execPath, [...flags, '--input-type=module', '-e', probe], {
    cwd: new URL('../', import.meta.url),
    env,
    stdio: ['ignore', 'pipe', 'pipe'],
    timeout: 8000,
  });
  let stdout = '';
  let stderr = '';
  child.stdout.on('data', (data) => (stdout += data));
  child.stderr.on('data', (data) => (stderr += data));
  const [code, signal] = await once(child, 'close');
  assert.equal(signal, null, stderr);
  assert.equal(code, 0, stderr);
  assert.doesNotMatch(stdout + stderr, /proxy-password|device-token-tttt/);
  return JSON.parse(stdout.trim());
}

async function directTarget(t) {
  const target = await server(
    t,
    (_request, response) => response.end('ok'),
    (_request, socket) => socket.end('HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n'),
  );
  return `http://127.0.0.1:${target.address().port}`;
}

test('ambient proxy URLs do not enable proxying or override local Node connection settings', async (t) => {
  const origin = await directTarget(t);
  const result = await runProbe(origin, {
    HTTP_PROXY: 'http://127.0.0.1:1',
    HTTPS_PROXY: 'http://127.0.0.1:1',
    NODE_OPTIONS: '--network-family-autoselection-attempt-timeout=733',
  });
  assert.equal(result.status, 200);
  assert.match(result.error, /HTTP 401/);
  assert.equal(result.timeout, 733);
  assert.deepEqual(result.overrides, [null]);
  assert.deepEqual(result.inheritedProxy, []);
});

test(
  'local startup opt-in sends both fetch and WebSocket requests through the proxy',
  proxyOptions,
  async (t) => {
    const requests = [];
    const tunnels = [];
    const proxy = await server(
      t,
      (request, response) => {
        requests.push(request.url);
        response.writeHead(502).end();
      },
      (request, socket) => {
        requests.push(request.url);
        socket.end('HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n');
      },
    );
    proxy.on('connect', (request, socket, head) => {
      tunnels.push(request.url);
      socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      proxy.emit('connection', socket);
      if (head.length) {
        socket.unshift(head);
      }
    });
    const result = await runProbe('http://unresolvable-agent.invalid', {
      NODE_USE_ENV_PROXY: '1',
      HTTP_PROXY: `http://127.0.0.1:${proxy.address().port}`,
    });
    assert.equal(result.status, 502);
    assert.match(result.error, /HTTP 502/);
    assert.deepEqual(tunnels, ['unresolvable-agent.invalid:80']);
    assert.deepEqual(requests, ['/health', 'http://unresolvable-agent.invalid/agent/proxy-test']);
    assert.deepEqual(result.inheritedProxy, []);
  },
);

test(
  'NO_PROXY and the native CLI opt-in apply to fetch and WebSocket together',
  proxyOptions,
  async (t) => {
    const origin = await directTarget(t);
    const result = await runProbe(
      origin,
      { HTTP_PROXY: 'http://127.0.0.1:1', NO_PROXY: '127.0.0.1' },
      ['--use-env-proxy'],
    );
    assert.equal(result.status, 200);
    assert.match(result.error, /HTTP 401/);
  },
);

test(
  'HTTPS proxy CONNECT carries proxy authentication without the device bearer token',
  proxyOptions,
  async (t) => {
    const proxy = await server(t);
    let destination;
    let headers;
    proxy.on('connect', (request, socket) => {
      destination = request.url;
      headers = request.headers;
      socket.end('HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n');
    });
    const result = await runProbe('https://unresolvable-agent.invalid', {
      NODE_USE_ENV_PROXY: '1',
      HTTPS_PROXY: `http://proxy-user:proxy-password@127.0.0.1:${proxy.address().port}`,
    });
    assert.equal(destination, 'unresolvable-agent.invalid:443');
    assert.equal(headers.authorization, undefined);
    assert.equal(
      headers['proxy-authorization'],
      `Basic ${Buffer.from('proxy-user:proxy-password').toString('base64')}`,
    );
    assert.match(result.error, /Gateway connection failed/);
    assert.deepEqual(result.inheritedProxy, []);
  },
);
