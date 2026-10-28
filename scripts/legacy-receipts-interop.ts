// Test-only comparison of the legacy receipt equivalence and recovery rules.
import fs from 'node:fs';
import { OperationError, OperationReceipts } from '../src/operation-receipts.js';

const cases: Array<{ label: string; left: string; right: string }> = JSON.parse(
  fs.readFileSync(0, 'utf8'),
);
const id = 'a'.repeat(32);
const secondId = 'b'.repeat(32);
const thirdId = 'c'.repeat(32);
function attempt(action: () => boolean) {
  try {
    return action() ? 'new' : 'existing';
  } catch (error) {
    if (error instanceof OperationError) {
      return `${error.code}:${error.executionState}`;
    }
    throw error;
  }
}
function status(receipts: OperationReceipts, operationId: string, instance: string) {
  const { instance_id: _instance, ...rest } = receipts.status(operationId, instance);
  return rest;
}
const comparisons = cases.map(({ label, left, right }) => {
  const receipts = new OperationReceipts('fixture-token');
  receipts.begin(id, receipts.instanceId, 'write_file', JSON.parse(left));
  return {
    label,
    outcome: attempt(() =>
      receipts.begin(id, receipts.instanceId, 'write_file', JSON.parse(right)),
    ),
  };
});
const receipts = new OperationReceipts('fixture-token', 2);
receipts.begin(id, receipts.instanceId, 'write_file', { content: 'receipt-fixture-secret' });
receipts.begin(secondId, receipts.instanceId, 'start_process', { command: 'true' });
const capacity = attempt(() => receipts.begin(thirdId, receipts.instanceId, 'write_file', {}));
receipts.complete(id, 'succeeded', {
  job_id: 'job_1',
  sha256: 'f'.repeat(64),
  size_bytes: 12,
  exit_code: null,
  stdin_closed: true,
  output: 'receipt-fixture-secret',
});
receipts.complete(id, 'unknown');
receipts.complete(secondId, 'failed', {}, 'policy_root_readonly');
const settled = status(receipts, id, receipts.instanceId);
const pending = receipts.pending();
receipts.acknowledge(id, 'different-instance');
const afterWrongAck = receipts.pending();
receipts.acknowledge(secondId, receipts.instanceId);
const afterSecondAck = receipts.pending();
receipts.acknowledge(id, receipts.instanceId);
const afterAllAck = receipts.pending();
const replay = attempt(() =>
  receipts.begin(id, receipts.instanceId, 'write_file', { content: 'receipt-fixture-secret' }),
);
const changed = attempt(() =>
  receipts.begin(id, receipts.instanceId, 'write_file', { content: 'changed' }),
);
const oldInstance = status(receipts, id, 'different-instance');
const staleStart = attempt(() => receipts.begin(id, 'different-instance', 'write_file', {}));
console.log(
  JSON.stringify({
    comparisons,
    lifecycle: {
      capacity,
      settled,
      pending,
      afterWrongAck,
      afterSecondAck,
      afterAllAck,
      replay,
      changed,
      oldInstance,
      staleStart,
    },
  }),
);
