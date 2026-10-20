import { spawn } from 'node:child_process';
import process from 'node:process';
import { StringDecoder } from 'node:string_decoder';
import { randomBytes } from 'node:crypto';
import { buildBwrapArgs, limitsPrefix } from './sandbox.mjs';

const MAX_OUTPUT = 256 * 1024;
const MAX_JOBS = 64;
const CLOSE_WAIT_MS = 1000;
const START_WAIT_MS = 250;

const codePoints = (value) => Array.from(value);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const waitBounded = (promise, milliseconds) => Promise.race([promise, sleep(milliseconds)]);
const sliceCodePoints = (value, start, end) => codePoints(value).slice(start, end).join('');

export class Processes {
  constructor(config, files, options = {}) {
    this.config = config;
    this.files = files;
    this.sandbox = options.sandbox ?? null;
    this.jobs = new Map();
    this.startTail = Promise.resolve();
    this.offlineTimer = null;
    this.accepting = true;
  }

  async start(command, root, cwd = '.', timeout_seconds = 300, wait_seconds = 1) {
    if (!this.config.allow_shell) throw new Error('Shell execution is disabled in this device\'s configuration');
    if (typeof command !== 'string' || !command || command.length > 32768 || command.includes('\0')) {
      throw new Error('Invalid command');
    }
    if (!Number.isInteger(timeout_seconds) || timeout_seconds < 1 ||
        timeout_seconds > this.config.max_process_seconds ||
        typeof wait_seconds !== 'number' || wait_seconds < 0 || wait_seconds > 5) {
      throw new Error('Invalid timeout or wait; see device_info limits');
    }

    let release;
    const previous = this.startTail;
    if (!this.accepting) throw new Error('Process manager is closing or offline');
    this.startTail = new Promise((resolve) => { release = resolve; });
    await previous;
    try {
      if (!this.accepting) throw new Error('Process manager is closing or offline');
      if ([...this.jobs.values()].filter((job) => !job.closed).length >= this.config.max_processes) {
        throw new Error('Maximum concurrent processes reached');
      }
      if (this.jobs.size >= MAX_JOBS) {
        const old = [...this.jobs.entries()].find(([, job]) => job.closed);
        if (!old) throw new Error('Maximum retained process jobs reached');
        this.jobs.delete(old[0]);
      }

      const workdir = await this.files.checkedDirectory(root, cwd);
      if (!this.accepting) throw new Error('Process manager is closing or offline');
      const env = {};
      for (const key of ['PATH', 'HOME', 'USER', 'LANG', 'LC_ALL', 'TMPDIR']) {
        if (process.env[key] !== undefined) env[key] = process.env[key];
      }
      if (!env.PATH) env.PATH = '/usr/local/bin:/usr/bin:/bin';
      // Python's subprocess uses stderr=STDOUT, so use one pipe to preserve
      // the arrival order of stdout and stderr chunks.
      // Without sandbox settings this is exactly the previous behaviour.
      const script = `${limitsPrefix(this.sandbox?.limits)}exec 2>&1; ${command}`;
      let executable = this.config.shell;
      let argv = ['-c', script];
      if (this.sandbox?.mode === 'bwrap') {
        const writableRoots = new Set(this.config.allow_write
          ? Object.keys(this.files.roots).filter((name) => !this.sandbox.readonlyRoots.includes(name)) : []);
        executable = 'bwrap';
        argv = buildBwrapArgs({ roots: this.files.roots, writableRoots, workdir, network: this.sandbox.network,
          hideHome: this.sandbox.hideHome, home: this.sandbox.home, expose: this.sandbox.expose,
          hiddenPaths: this.sandbox.hiddenPaths, hiddenFiles: this.sandbox.hiddenFiles, shell: this.config.shell, script });
      }
      const child = spawn(executable, argv, {
        cwd: workdir,
        env,
        detached: true,
        shell: false,
        stdio: ['pipe', 'pipe', 'ignore'],
      });
      // Prevent an asynchronous EPIPE from becoming an uncaught exception;
      // writeStdin installs a scoped listener when it is waiting for drain.
      child.stdin.on('error', () => {});
      const job = {
        process: child,
        started: Date.now() / 1000,
        segments: [],
        output_length: 0,
        base_offset: 0,
        status: 'running',
        task: null,
        closed: false,
        stdinLock: Promise.resolve(),
      };
      const jobId = randomBytes(12).toString('hex');
      this.jobs.set(jobId, job);
      job.task = this.watch(job, timeout_seconds).finally(() => { job.closed = true; });
      // The creation lock must not serialize the caller's optional wait.
      release();
      release = null;
      if (wait_seconds) await Promise.race([job.task, sleep(wait_seconds * 1000)]);
      return this.read(jobId);
    } finally {
      if (release) release();
    }
  }

  // A transient gateway outage must not immediately kill local jobs, but an
  // agent that stays offline forever must not leave them running forever.
  markOffline(graceMs = 5 * 60 * 1000) {
    if (this.offlineTimer) return;
    this.accepting = false;
    this.offlineTimer = setTimeout(() => { this.offlineTimer = null; void this.close(); }, graceMs);
    this.offlineTimer.unref?.();
  }

  markOnline() {
    if (this.offlineTimer) clearTimeout(this.offlineTimer);
    this.offlineTimer = null;
    this.accepting = true;
  }

  append(job, text) {
    if (!text) return;
    const length = codePoints(text).length;
    job.segments.push({ text, length });
    job.output_length += length;
    while (job.output_length > MAX_OUTPUT && job.segments.length) {
      const excess = job.output_length - MAX_OUTPUT;
      const first = job.segments[0];
      if (first.length <= excess) {
        job.segments.shift();
        job.output_length -= first.length;
        job.base_offset += first.length;
      } else {
        first.text = sliceCodePoints(first.text, excess);
        first.length -= excess;
        job.output_length -= excess;
        job.base_offset += excess;
      }
    }
  }

  async watch(job, timeoutSeconds) {
    const decoder = new StringDecoder('utf8');
    job.process.stdout.on('data', (chunk) => this.append(job, decoder.write(chunk)));
    const closed = new Promise((resolve) => {
      job.process.once('close', resolve);
      job.process.once('error', () => resolve(null));
    });
    let timer;
    try {
      const result = await Promise.race([
        closed.then((code) => ({ kind: 'closed', code })),
        new Promise((resolve) => { timer = setTimeout(() => resolve({ kind: 'timeout' }), timeoutSeconds * 1000); }),
      ]);
      if (result.kind === 'closed') {
        if (job.status === 'running') job.status = 'exited';
      } else {
        job.status = 'timed_out';
      }
    } catch (error) {
      if (job.status === 'running') job.status = 'terminated';
      throw error;
    } finally {
      clearTimeout(timer);
      this.append(job, decoder.end());
      this.kill(job);
      await closed;
    }
  }

  kill(job) {
    if (!job.process.pid) return;
    try { process.kill(-job.process.pid, 'SIGKILL'); }
    catch (error) { if (error.code !== 'ESRCH') { try { job.process.kill('SIGKILL'); } catch {} } }
  }

  read(job_id, offset = 0, limit = 32768) {
    if (!this.jobs.has(job_id)) throw new Error('Unknown process job');
    if (!Number.isInteger(offset) || offset < 0 || !Number.isInteger(limit) || limit < 1 || limit > 65536) {
      throw new Error('Invalid offset/limit');
    }
    const job = this.jobs.get(job_id);
    const start = Math.max(offset, job.base_offset);
    let relative = start - job.base_offset;
    let remaining = limit;
    const output = [];
    for (const segment of job.segments) {
      if (relative >= segment.length) { relative -= segment.length; continue; }
      const take = Math.min(remaining, segment.length - relative);
      output.push(sliceCodePoints(segment.text, relative, relative + take));
      remaining -= take;
      relative = 0;
      if (!remaining) break;
    }
    const content = output.join('');
    return { job_id, pid: job.process.pid, status: job.status, exit_code: job.process.exitCode,
      cleanup_pending: job.status !== 'running' && !job.closed,
      output: content, offset: start, next_offset: start + codePoints(content).length,
      output_dropped: offset < job.base_offset, started_at: job.started };
  }

  list() {
    return { jobs: [...this.jobs.entries()].map(([job_id, job]) => ({ job_id, pid: job.process.pid,
      status: job.status, exit_code: job.process.exitCode, started_at: job.started,
      cleanup_pending: job.status !== 'running' && !job.closed })) };
  }

  async writeStdin(job_id, text = '', close = false) {
    const job = this.jobs.get(job_id);
    if (!job || job.status !== 'running' || typeof text !== 'string' || Buffer.byteLength(text) > 16384) {
      throw new Error('Process is not running or input exceeds 16 KiB');
    }
    let release;
    const previous = job.stdinLock;
    job.stdinLock = new Promise((resolve) => { release = resolve; });
    await previous;
    try {
      if (job.status !== 'running') throw new Error('Process is not running or input exceeds 16 KiB');
      if (text && !job.process.stdin.write(text)) {
        await new Promise((resolve, reject) => {
          let timer;
          const done = (error) => {
            clearTimeout(timer);
            job.process.stdin.removeListener('drain', onDrain);
            job.process.stdin.removeListener('error', onError);
            job.process.removeListener('close', onClose);
            if (error) reject(error); else resolve();
          };
          const onDrain = () => done();
          const onError = (error) => done(error);
          const onClose = () => done(new Error('Process is not running or input exceeds 16 KiB'));
          timer = setTimeout(() => done(new Error('Process stdin did not drain within 3 seconds')), 3000);
          job.process.stdin.once('drain', onDrain);
          job.process.stdin.once('error', onError);
          job.process.once('close', onClose);
        });
      }
      if (close) job.process.stdin.end();
    } finally { release(); }
    return { job_id, sent: codePoints(text).length, stdin_closed: close };
  }

  async terminate(job_id) {
    const job = this.jobs.get(job_id);
    if (!job) throw new Error('Unknown process job');
    if (!job.closed) {
      if (job.status === 'running') job.status = 'terminated';
      this.kill(job);
      await waitBounded(job.task, CLOSE_WAIT_MS);
    }
    return this.read(job_id);
  }

  async close() {
    this.accepting = false;
    const pendingStart = this.startTail;
    const stopping = [...this.jobs.values()]
      .filter((job) => !job.closed)
      .map((job) => {
        if (job.status === 'running') job.status = 'terminated';
        this.kill(job);
        return waitBounded(job.task, CLOSE_WAIT_MS);
      });
    if (this.offlineTimer) clearTimeout(this.offlineTimer);
    this.offlineTimer = null;
    await Promise.all([...stopping, waitBounded(pendingStart, START_WAIT_MS)]);
  }
}
