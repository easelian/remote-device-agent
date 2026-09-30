import assert from 'node:assert/strict';
import test from 'node:test';
import { Processes } from '../lib/processes.mjs';

const config = (overrides = {}) => ({ allow_shell: true, shell: '/bin/sh', max_processes: 4,
  max_process_seconds: 10, ...overrides });
const files = { checkedDirectory: async (_root, cwd) => cwd === '.' ? process.cwd() : cwd };

test('captures output and reports Unicode code point offsets', async () => {
  const processes = new Processes(config(), files);
  const job = await processes.start("printf 'A😀B'", 'work', '.', 3, 1);
  const first = processes.read(job.job_id, 1, 1);
  assert.equal(first.output, '😀');
  assert.equal(first.next_offset, 2);
  assert.equal(processes.read(job.job_id, 2, 1).output, 'B');
  await processes.close();
});

test('caps output in code points and marks discarded offsets', async () => {
  const processes = new Processes(config(), files);
  const job = await processes.start("printf '%*s' 262150 '' | tr ' ' x", 'work', '.', 3, 2);
  const result = processes.read(job.job_id, 0, 1);
  assert.equal(result.output_dropped, true);
  assert.equal(result.offset, 6);
  assert.equal(result.output.length, 1);
  await processes.close();
});

test('sends stdin and closes it', async () => {
  const processes = new Processes(config(), files);
  const job = await processes.start('cat', 'work', '.', 3, 0);
  await processes.writeStdin(job.job_id, 'hello😀', true);
  await jobWait(processes, job.job_id);
  assert.equal(processes.read(job.job_id).output, 'hello😀');
  await processes.close();
});

test('times out and kills the process group', async () => {
  const processes = new Processes(config(), files);
  const job = await processes.start('sleep 30', 'work', '.', 1, 2);
  assert.equal(job.status, 'timed_out');
  await processes.close();
});

test('rejects shell and concurrent process limits', async () => {
  const processes = new Processes(config({ allow_shell: false }), files);
  await assert.rejects(() => processes.start('true', 'work'));
  const limited = new Processes(config({ max_processes: 1 }), files);
  const job = await limited.start('sleep 2', 'work', '.', 5, 0);
  await assert.rejects(() => limited.start('true', 'work', '.', 5), /Maximum concurrent/);
  await limited.terminate(job.job_id);
});

test('does not hold the creation lock during the optional wait', async () => {
  const processes = new Processes(config({ max_processes: 2 }), files);
  const first = processes.start('sleep 2', 'work', '.', 5, 2);
  await new Promise((resolve) => setTimeout(resolve, 40));
  const started = performance.now();
  const second = await processes.start('true', 'work', '.', 5, 0.1);
  assert.equal(second.status, 'exited');
  assert.ok(performance.now() - started < 1000);
  await first;
  await processes.close();
});

test('bounds stdin backpressure wait to three seconds', async () => {
  const processes = new Processes(config(), files);
  const job = await processes.start('sleep 5', 'work', '.', 10, 0);
  const started = performance.now();
  let error;
  for (let i = 0; i < 64 && !error; i++) {
    try { await processes.writeStdin(job.job_id, 'x'.repeat(16384)); }
    catch (caught) { error = caught; }
  }
  assert.match(error?.message ?? '', /did not drain|EPIPE|not running/);
  assert.ok(performance.now() - started >= 2500 && performance.now() - started < 4500);
  await processes.terminate(job.job_id);
});

async function jobWait(processes, id) {
  for (let i = 0; i < 50; i++) {
    if (processes.read(id).status !== 'running') return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error('job did not finish');
}
