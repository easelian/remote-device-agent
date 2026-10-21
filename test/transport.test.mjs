import test from 'node:test';
import assert from 'node:assert/strict';
import { BoundedSender } from '../.build/transport.js';

function socket(write) {
  return {
    readyState: 1,
    bufferedAmount: 0,
    terminated: false,
    send: write,
    terminate() {
      this.terminated = true;
      this.readyState = 3;
    },
  };
}

test('transport serializes writes and releases its byte and message budgets', async () => {
  let active = 0;
  let maximum = 0;
  const peer = socket((_text, callback) => {
    maximum = Math.max(maximum, ++active);
    setTimeout(() => {
      active -= 1;
      callback();
    }, 2);
  });
  const sender = new BoundedSender(peer);
  await Promise.all(
    Array.from({ length: 8 }, (_, id) =>
      sender.send({ type: 'result', id, result: 'a'.repeat(10000) }),
    ),
  );
  assert.equal(maximum, 1);
  assert.equal(sender.queuedBytes, 0);
  assert.equal(sender.queuedMessages, 0);
});

test('a peer that never drains has a send deadline and cannot accumulate unbounded data', async () => {
  const peer = socket(() => {});
  const sender = new BoundedSender(peer, { sendTimeoutMs: 20 });
  const started = Date.now();
  await assert.rejects(sender.send({ type: 'result', id: 'one', result: 'x' }), /timed out/);
  assert.ok(Date.now() - started < 500);
  assert.equal(peer.terminated, true);
  assert.equal(sender.queuedBytes, 0);
});

test('transport terminates on a full queue and settles all queued writes', async () => {
  const peer = socket(() => {});
  const sender = new BoundedSender(peer, { maxQueuedMessages: 2 });
  const pending = [sender.send({ type: 'heartbeat' }), sender.send({ type: 'heartbeat' })];
  await assert.rejects(sender.send({ type: 'heartbeat' }), /budget/);
  const results = await Promise.allSettled(pending);
  assert.equal(
    results.every((item) => item.status === 'rejected'),
    true,
  );
  assert.equal(peer.terminated, true);
  assert.equal(sender.queuedBytes, 0);
  assert.equal(sender.queuedMessages, 0);
});

test('oversized results become a correlated bounded error', async () => {
  const messages = [];
  const peer = socket((text, callback) => {
    messages.push(JSON.parse(text));
    callback();
  });
  const sender = new BoundedSender(peer, { maxFrameBytes: 200 });
  await sender.send({ type: 'result', id: 'large', result: 'x'.repeat(1000) });
  assert.equal(messages[0].id, 'large');
  assert.match(messages[0].error, /transport limit/);
  assert.equal(peer.terminated, false);
});

test('socket buffering is included in admission and close cancels a stalled writer', async () => {
  const peer = socket(() => {});
  const sender = new BoundedSender(peer, { maxBufferedBytes: 200 });
  peer.bufferedAmount = 199;
  await assert.rejects(sender.send({ type: 'heartbeat' }), /budget/);
  assert.equal(peer.terminated, true);
  const other = socket(() => {});
  const writer = new BoundedSender(other);
  const pending = writer.send({ type: 'heartbeat' });
  await Promise.resolve();
  writer.close();
  await assert.rejects(pending, /closed/);
  assert.equal(writer.queuedBytes, 0);
});
