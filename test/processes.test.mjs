import assert from 'node:assert/strict';
import test from 'node:test';
import { Processes } from '../.build/processes.js';

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

test('keeps event loop responsive while bounding a 100 MiB stdout burst', { timeout: 30000 }, async () => {
  const processes = new Processes(config(), files);
  let maximumTickDelay = 0;
  let previous = performance.now();
  const ticker = setInterval(() => {
    const now = performance.now();
    maximumTickDelay = Math.max(maximumTickDelay, now - previous);
    previous = now;
  }, 10);
  try {
    const job = await processes.start("dd if=/dev/zero bs=65536 count=1600 2>/dev/null | tr '\\0' x", 'work', '.', 10, 0);
    let result;
    do { await new Promise((resolve) => setTimeout(resolve, 20)); result = processes.read(job.job_id); }
    while (result.status === 'running');
    const retained = processes.read(job.job_id, 0, 65536);
    assert.equal(result.status, 'exited');
    assert.equal(retained.output.length, 65536);
    assert.equal(retained.output_dropped, true);
    assert.ok(retained.offset > 0);
    assert.ok(maximumTickDelay < 500, `event loop stalled for ${maximumTickDelay}ms`);
  } finally {
    clearInterval(ticker);
    await processes.close();
  }
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

test('offline grace starts before a late job and rejects it', async () => {
  const processes = new Processes(config(), files);
  processes.markOffline(80);
  await new Promise((resolve) => setTimeout(resolve, 30));
  await assert.rejects(() => processes.start('sleep 30', 'work', '.', 10, 0), /offline/);
  processes.markOffline(200);
  await new Promise((resolve) => setTimeout(resolve, 130));
  await processes.close();
});

test('close prevents a delayed start from spawning after directory validation', async () => {
  const delayedFiles = { checkedDirectory: async () => {
    await new Promise((resolve) => setTimeout(resolve, 60));
    return process.cwd();
  } };
  const processes = new Processes(config(), delayedFiles);
  const starting = processes.start('sleep 30', 'work', '.', 10, 0);
  await new Promise((resolve) => setTimeout(resolve, 5));
  const closing = processes.close();
  await assert.rejects(starting, /closing|offline/);
  await closing;
  assert.equal(processes.jobs.size, 0);
});

test('close is bounded when directory validation never resolves', async () => {
  let releaseDirectory;
  const delayedFiles = { checkedDirectory: () => new Promise((resolve) => { releaseDirectory = resolve; }) };
  const processes = new Processes(config(), delayedFiles);
  const starting = processes.start('sleep 30', 'work', '.', 10, 0);
  await new Promise((resolve) => setTimeout(resolve, 5));
  const started = performance.now();
  await processes.close();
  assert.ok(performance.now() - started < 600, 'close waited for directory validation');
  releaseDirectory(process.cwd());
  await assert.rejects(starting, /closing|offline/);
  assert.equal(processes.jobs.size, 0);
});

test('close kills an existing job without waiting indefinitely for its watcher', async () => {
  const processes = new Processes(config(), files);
  const job = await processes.start('sleep 30', 'work', '.', 10, 0);
  const started = performance.now();
  await processes.close();
  assert.ok(performance.now() - started < 1500, 'close exceeded bounded child cleanup');
  assert.notEqual(processes.read(job.job_id).status, 'running');
  assert.throws(() => process.kill(job.pid, 0), (error) => error.code === 'ESRCH');
});

test('termination reports pending cleanup until the actual watcher closes', async () => {
  const processes = new Processes(config(), files);
  let finish;
  const pending = new Promise((resolve) => { finish = resolve; });
  processes.jobs.set('pending', { process: { pid: null, exitCode: null }, status: 'running',
    closed: false, task: pending, segments: [], base_offset: 0, started: Date.now() });
  const result = await processes.terminate('pending');
  assert.equal(result.cleanup_pending, true);
  assert.equal(processes.list().jobs[0].cleanup_pending, true);
  processes.jobs.get('pending').closed = true;
  finish();
  assert.equal(processes.read('pending').cleanup_pending, false);
});

test('pending process cleanup retains the actual concurrency slot', async () => {
  const processes = new Processes(config({ max_processes: 1 }), files);
  const pending = { process: { pid: null, exitCode: null }, status: 'terminated', closed: false,
    task: new Promise(() => {}), segments: [], base_offset: 0, started: Date.now() };
  processes.jobs.set('pending', pending);
  await assert.rejects(() => processes.start('true', 'work', '.', 3, 0), /Maximum concurrent/);
  pending.closed = true;
  const job = await processes.start('true', 'work', '.', 3, 1);
  assert.equal(job.status, 'exited');
  await processes.close();
});

test('history eviction preserves a process whose watcher is still open', async () => {
  const processes = new Processes(config(), files);
  const pending = { process: { pid: null, exitCode: null }, status: 'terminated', closed: false,
    task: Promise.resolve(), segments: [], base_offset: 0, started: Date.now() };
  processes.jobs.set('pending', pending);
  for (let index = 0; index < 63; index++) processes.jobs.set(`old-${index}`, { ...pending, closed: true });
  await processes.start('true', 'work', '.', 3, 1);
  assert.equal(processes.jobs.size, 64);
  assert.equal(processes.jobs.get('pending'), pending);
  pending.closed = true;
  await processes.close();
});

async function jobWait(processes, id) {
  for (let i = 0; i < 50; i++) {
    if (processes.read(id).status !== 'running') return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error('job did not finish');
}
