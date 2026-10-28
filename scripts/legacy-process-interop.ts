// Test-only comparison against the legacy Agent's real process manager.
import { Files } from '../src/files.js';
import { Processes } from '../src/processes.js';

const root = process.argv[2];
if (!root) {
  throw new Error('The fixture requires a file root');
}
const config = {
  gateway_url: 'http://localhost',
  device_id: 'fixture',
  token: 'fixture',
  roots: { work: root },
  allow_shell: true,
  allow_write: true,
  shell: '/bin/sh',
  max_processes: 4,
  max_process_seconds: 10,
};
const manager = new Processes(config, new Files(config.roots, true));

function normalized(result: ReturnType<typeof manager.read>) {
  const { job_id: _id, pid: _pid, started_at: _started, ...rest } = result;
  return rest;
}
function read(id: string, offset?: unknown, limit?: unknown) {
  try {
    return normalized(manager.read(id, offset, limit));
  } catch (error) {
    return { failure: error instanceof Error ? error.message : String(error) };
  }
}
async function complete(id: string) {
  for (let attempt = 0; attempt < 1000; attempt++) {
    const result = manager.read(id);
    if (result.status !== 'running' && !result.cleanup_pending) {
      return normalized(result);
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('Fixture process did not finish');
}

try {
  const job = await manager.start("printf 'A😀B\\nlast'; exit 7", 'work', '.', 10, 2);
  await complete(job.job_id);
  const cursors = {
    full: read(job.job_id),
    emoji: read(job.job_id, 1, 1),
    end: read(job.job_id, 30, 2),
    large: read(job.job_id, 2 ** 53, 1),
    huge: read(job.job_id, 1e100, 1),
    maximum: read(job.job_id, Number.MAX_VALUE, 1),
    negative: read(job.job_id, -1),
    fractional: read(job.job_id, 0.5),
    null_offset: read(job.job_id, null),
    null_limit: read(job.job_id, 0, null),
    zero_limit: read(job.job_id, 0, 0),
    unknown: read('unknown'),
  };
  const cat = await manager.start('dd bs=1 count=7 2>/dev/null', 'work', '.', 10, 0);
  const sent = await manager.writeStdin(cat.job_id, 'go😀\n', true);
  const { job_id: _id, ...input } = sent;
  const echoed = await complete(cat.job_id);
  const overflow = await manager.start(
    "dd if=/dev/zero bs=262150 count=1 2>/dev/null | tr '\\000' x",
    'work',
    '.',
    10,
    3,
  );
  await complete(overflow.job_id);
  const dropped = read(overflow.job_id, 0, 1);
  const tail = read(overflow.job_id, 262149, 2);
  console.log(JSON.stringify({ cursors, input, echoed, dropped, tail }));
} finally {
  await manager.close();
}
