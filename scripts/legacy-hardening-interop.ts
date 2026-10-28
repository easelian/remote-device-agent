// Test-only access to the legacy Agent's actual device hardening report.
import { Agent } from '../src/agent.js';
import { Policy } from '../src/policy.js';

const root = process.argv[2];
if (!root) {
  throw new Error('The fixture requires a file root');
}
const config = {
  gateway_url: 'http://localhost',
  device_id: 'fixture',
  token: 'fixture-token-01234567890123456789',
  roots: { work: root },
  allow_shell: true,
  allow_write: true,
  shell: '/bin/sh',
  max_processes: 4,
  max_process_seconds: 10,
};
const policy = new Policy({
  allowed_operations: ['read_file', 'write_file'],
  readonly_roots: ['work'],
  protected_paths: ['.env', 'private/key'],
});
const agent = new Agent(config, { policy });
const firstOrder = new Agent({ ...config, roots: { work: root, z: root, a: root } }, { policy });
const secondOrder = new Agent({ ...config, roots: { work: root, a: root, z: root } }, { policy });
try {
  console.log(
    JSON.stringify({
      report: agent.hardeningInfo(),
      reordered_roots_revisions_differ:
        firstOrder.hardeningInfo().policy_revision !== secondOrder.hardeningInfo().policy_revision,
    }),
  );
} finally {
  await Promise.all([agent.close(), firstOrder.close(), secondOrder.close()]);
}
