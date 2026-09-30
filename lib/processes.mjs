import { spawn } from 'node:child_process';
import process from 'node:process';
import { StringDecoder } from 'node:string_decoder';
import { randomBytes } from 'node:crypto';

const MAX_OUTPUT = 256 * 1024;
const MAX_JOBS = 64;

const codePoints = (value) => Array.from(value);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export class Processes {
  constructor(config, files) {
    this.config = config;
    this.files = files;
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
    this.startTail = new Promise((resolve) => { release = resolve; });
    await previous;
    try {
      if (!this.accepting) throw new Error('Process manager is closing or offline');
      if ([...this.jobs.values()].filter((job) => job.status === 'running').length >= this.config.max_processes) {
        throw new Error('Maximum concurrent processes reached');
      }
      if (this.jobs.size >= MAX_JOBS) {
        const old = [...this.jobs.entries()].find(([, job]) => job.status !== 'running');
        if (old) this.jobs.delete(old[0]);
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
      const child = spawn(this.config.shell, ['-c', `exec 2>&1; ${command}`], {
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
        output: '',
        base_offset: 0,
        status: 'running',
        task: null,
        stdinLock: Promise.resolve(),
      };
      const jobId = randomBytes(12).toString('hex');
      this.jobs.set(jobId, job);
      job.task = this.watch(job, timeout_seconds);
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
    const output = codePoints(job.output);
    output.push(...codePoints(text));
    if (output.length > MAX_OUTPUT) {
      const dropped = output.length - MAX_OUTPUT;
      job.base_offset += dropped;
      job.output = output.slice(dropped).join('');
    } else {
      job.output = output.join('');
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
    const points = codePoints(job.output);
    const start = Math.max(offset, job.base_offset);
    const output = points.slice(start - job.base_offset, start - job.base_offset + limit).join('');
    return { job_id, pid: job.process.pid, status: job.status, exit_code: job.process.exitCode,
      output, offset: start, next_offset: start + codePoints(output).length,
      output_dropped: offset < job.base_offset, started_at: job.started };
  }

  list() {
    return { jobs: [...this.jobs.entries()].map(([job_id, job]) => ({ job_id, pid: job.process.pid,
      status: job.status, exit_code: job.process.exitCode, started_at: job.started })) };
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
    if (job.status === 'running') {
      job.status = 'terminated';
      this.kill(job);
      await job.task;
    }
    return this.read(job_id);
  }

  async close() {
    this.accepting = false;
    await this.startTail;
    if (this.offlineTimer) clearTimeout(this.offlineTimer);
    this.offlineTimer = null;
    await Promise.all([...this.jobs.keys()].map((job_id) => this.terminate(job_id).catch(() => undefined)));
  }
}
