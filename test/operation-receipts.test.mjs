import assert from 'node:assert/strict';
import test from 'node:test';
import { OperationReceipts } from '../.build/operation-receipts.js';

test('receipts reject changed parameters and retain only recovery metadata', () => {
  const receipts = new OperationReceipts('secret-token', 2);
  const id = 'a'.repeat(32);
  assert.equal(
    receipts.begin(id, receipts.instanceId, 'write_file', { content: 'secret', path: 'private' }),
    true,
  );
  assert.equal(
    receipts.begin(id, receipts.instanceId, 'write_file', { path: 'private', content: 'secret' }),
    false,
  );
  assert.throws(
    () => receipts.begin(id, receipts.instanceId, 'write_file', { content: 'changed' }),
    { code: 'operation_conflict' },
  );
  receipts.complete(id, 'succeeded', {
    sha256: 'f'.repeat(64),
    size_bytes: 4,
    path: 'private',
    output: 'secret',
  });
  receipts.complete(id, 'unknown');
  const status = receipts.status(id, receipts.instanceId);
  assert.equal(status.execution_state, 'succeeded');
  assert.deepEqual(status.result_summary, { sha256: 'f'.repeat(64), size_bytes: 4 });
  assert.doesNotMatch(JSON.stringify(status), /private|secret/);
  assert.deepEqual(receipts.pending(), [id]);
  receipts.acknowledge(id, receipts.instanceId);
  assert.deepEqual(receipts.pending(), []);
  assert.equal(
    receipts.begin(id, receipts.instanceId, 'write_file', { content: 'secret', path: 'private' }),
    false,
  );
});

test('running receipts consume bounded capacity and old instances cannot dispatch', async () => {
  const receipts = new OperationReceipts('token', 1, 1);
  const id = 'a'.repeat(32);
  receipts.begin(id, receipts.instanceId, 'write_file', {});
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.throws(() => receipts.begin('b'.repeat(32), receipts.instanceId, 'write_file', {}), {
    code: 'operation_capacity',
  });
  const restarted = new OperationReceipts('token');
  assert.equal(restarted.status(id, receipts.instanceId).execution_state, 'unknown');
  assert.throws(() => restarted.begin(id, receipts.instanceId, 'write_file', {}), {
    code: 'operation_instance_changed',
  });
  receipts.complete(id, 'succeeded');
  await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(receipts.status(id, receipts.instanceId).execution_state, 'unknown');
});
