import test from 'node:test';
import assert from 'node:assert/strict';
import { DIRECT_SERVER, FALLBACK_SERVER, sameHub, selectServer } from '../.build/server-selection.js';

test('healthy direct endpoint wins without contacting fallback', async () => {
  const calls = [];
  assert.equal(await selectServer({ fetchImpl: async (url, options) => {
    calls.push(url);
    assert.equal(options.redirect, 'error');
    return Response.json({ status: 'ok', service: 'remote-device-hub' });
  } }), DIRECT_SERVER);
  assert.deepEqual(calls, [`${DIRECT_SERVER}/healthz`]);
});

test('connection errors, HTTP failures, and unrelated services use fallback', async () => {
  for (const fetchImpl of [async () => { throw new Error('TLS/connect failure'); },
    async () => new Response('', { status: 503 }),
    async () => Response.json({ status: 'ok', service: 'other' }),
    async () => new Response('not json')]) {
    assert.equal(await selectServer({ fetchImpl }), FALLBACK_SERVER);
  }
});

test('500ms deadline includes response body and aborts the probe', async () => {
  let signal;
  const started = performance.now();
  const selected = await selectServer({ fetchImpl: async (_url, options) => {
    signal = options.signal;
    return { ok: true, json: () => new Promise(() => {}) };
  } });
  assert.equal(selected, FALLBACK_SERVER);
  assert.equal(signal.aborted, true);
  assert(performance.now() - started >= 450);
  assert(performance.now() - started < 1500);
});

test('user cancellation does not become fallback success', async () => {
  const controller = new AbortController(); controller.abort(new Error('stopped'));
  await assert.rejects(selectServer({ signal: controller.signal }), /stopped/);
});

test('only the two exact deployment origins share trust', () => {
  assert(sameHub(DIRECT_SERVER, FALLBACK_SERVER));
  assert(sameHub(FALLBACK_SERVER, DIRECT_SERVER));
  for (const value of ['https://evil.test', 'http://xdistribute.site:8090',
    'https://mcp.xdistribute.site.evil.test', 'https://xdistribute.site:8443']) {
    assert.equal(sameHub(DIRECT_SERVER, value), false);
    assert.equal(sameHub(FALLBACK_SERVER, value), false);
  }
});
