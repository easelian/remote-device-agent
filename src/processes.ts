import { spawn } from 'node:child_process';
import process from 'node:process';
import { StringDecoder } from 'node:string_decoder';
import { randomBytes } from 'node:crypto';
import { buildBwrapArgs, limitsPrefix } from './sandbox.js';
import type { DeviceConfig, SandboxSettings } from './types.js';
import type { ChildProcessByStdio } from 'node:child_process';
import type { Writable, Readable } from 'node:stream';
import { errorCode, isInteger, type Awaitable } from './boundary.js';

interface ProcessJob {
  process: ChildProcessByStdio<Writable, Readable, null>;
  started: number;
  segments: { text: string; length: number }[];
  output_length: number;
  base_offset: number;
  status: 'running' | 'exited' | 'timed_out' | 'terminated';
  task: Promise<void> | null;
  closed: boolean;
  stdinLock: Promise<void>;
}

const MAX_OUTPUT = 256 * 1024;
const MAX_JOBS = 64;
const CLOSE_WAIT_MS = 1000;
const START_WAIT_MS = 250;

const codePoints = (value: string) => Array.from(value);
const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const waitBounded = <T>(promise: Awaitable<T>, milliseconds: number) =>
  Promise.race([promise, sleep(milliseconds)]);
const sliceCodePoints = (value: string, start: number, end?: number) =>
  codePoints(value).slice(start, end).join('');

export class Processes {
  config: DeviceConfig;
  files: {
    checkedDirectory(root: unknown, cwd: unknown): Promise<string>;
    roots: Record<string, string>;
  };
  sandbox: SandboxSettings | null;
  jobs: Map<string, ProcessJob>;
  startTail: Promise<void>;
  offlineTimer: ReturnType<typeof setTimeout> | null;
  accepting: boolean;
  constructor(
    config: DeviceConfig,
    files: Processes['files'],
    options: { sandbox?: SandboxSettings | null } = {},
  ) {
    this.config = config;
    this.files = files;
    this.sandbox = options.sandbox ?? null;
    this.jobs = new Map();
    this.startTail = Promise.resolve();
    this.offlineTimer = null;
    this.accepting = true;
  }

  async start(
    command: unknown,
    root: unknown,
    cwd: unknown = '.',
    timeout_seconds: unknown = 300,
    wait_seconds: unknown = 1,
  ) {
    if (!this.config.allow_shell) {
      throw new Error("Shell execution is disabled in this device's configuration");
    }
    if (
      typeof command !== 'string' ||
      !command ||
      command.length > 32768 ||
      command.includes('\0')
    ) {
      throw new Error('Invalid command');
    }
    if (
      !isInteger(timeout_seconds) ||
      timeout_seconds < 1 ||
      timeout_seconds > this.config.max_process_seconds ||
      typeof wait_seconds !== 'number' ||
      wait_seconds < 0 ||
      wait_seconds > 5
    ) {
      throw new Error('Invalid timeout or wait; see device_info limits');
    }

    const release: { value: (() => void) | null } = { value: null };
    const previous = this.startTail;
    if (!this.accepting) {
      throw new Error('Process manager is closing or offline');
    }
    this.startTail = new Promise<void>((resolve) => {
      release.value = resolve;
    });
    await previous;
    try {
      if (!this.accepting) {
        throw new Error('Process manager is closing or offline');
      }
      if (
        [...this.jobs.values()].filter((job) => !job.closed).length >= this.config.max_processes
      ) {
        throw new Error('Maximum concurrent processes reached');
      }
      if (this.jobs.size >= MAX_JOBS) {
        const old = [...this.jobs.entries()].find(([, job]) => job.closed);
        if (!old) {
          throw new Error('Maximum retained process jobs reached');
        }
        this.jobs.delete(old[0]);
      }

      const workdir = await this.files.checkedDirectory(root, cwd);
      if (!this.accepting) {
        throw new Error('Process manager is closing or offline');
      }
      const env: NodeJS.ProcessEnv = {};
      for (const key of [
        'PATH',
        'HOME',
        'USER',
        'LANG',
        'LC_ALL',
        'TMPDIR',
        ...(process.platform === 'win32'
          ? [
              'Path',
              'SystemRoot',
              'SYSTEMROOT',
              'WINDIR',
              'COMSPEC',
              'PATHEXT',
              'TEMP',
              'TMP',
              'USERPROFILE',
              'APPDATA',
              'LOCALAPPDATA',
            ]
          : []),
      ]) {
        if (process.env[key] !== undefined) {
          env[key] = process.env[key];
        }
      }
      if (!env.PATH && !env.Path) {
        env.PATH =
          process.platform === 'win32'
            ? `${process.env.SystemRoot || 'C:\\Windows'}\\System32`
            : '/usr/local/bin:/usr/bin:/bin';
      }
      // Python's subprocess uses stderr=STDOUT, so use one pipe to preserve
      // the arrival order of stdout and stderr chunks.
      // Without sandbox settings this is exactly the previous behaviour.
      if (
        process.platform === 'win32' &&
        (this.sandbox?.mode === 'bwrap' || Object.keys(this.sandbox?.limits ?? {}).length)
      ) {
        throw new Error('Configured POSIX sandbox/resource limits are unavailable on Windows');
      }
      const windows = process.platform === 'win32';
      const powershell =
        windows &&
        /^(?:powershell|pwsh)(?:\.exe)?$/i.test(this.config.shell.split(/[\\/]/).at(-1) || '');
      if (
        windows &&
        !powershell &&
        !/^cmd(?:\.exe)?$/i.test(this.config.shell.split(/[\\/]/).at(-1) || '')
      ) {
        throw new Error('Windows shell must be cmd.exe, powershell.exe or pwsh.exe');
      }
      const script = windows
        ? `${command} 2>&1`
        : `${limitsPrefix(this.sandbox?.limits)}exec 2>&1; ${command}`;
      let executable = this.config.shell;
      let argv = windows
        ? powershell
          ? ['-NoLogo', '-NoProfile', '-NonInteractive', '-Command', script]
          : ['/d', '/s', '/c', `"${script}"`]
        : ['-c', script];
      if (this.sandbox?.mode === 'bwrap') {
        const writableRoots = new Set(
          this.config.allow_write
            ? Object.keys(this.files.roots).filter(
                (name) => !this.sandbox?.readonlyRoots?.includes(name),
              )
            : [],
        );
        executable = 'bwrap';
        argv = buildBwrapArgs({
          roots: this.files.roots,
          writableRoots,
          workdir,
          network: this.sandbox.network,
          hideHome: this.sandbox.hideHome,
          home: this.sandbox.home,
          expose: this.sandbox.expose,
          hiddenPaths: this.sandbox.hiddenPaths,
          hiddenFiles: this.sandbox.hiddenFiles,
          shell: this.config.shell,
          script,
        });
      }
      const child = spawn(executable, argv, {
        cwd: workdir,
        env,
        detached: !windows,
        windowsHide: true,
        windowsVerbatimArguments: windows && !powershell,
        shell: false,
        stdio: ['pipe', 'pipe', 'ignore'],
      });
      // Prevent an asynchronous EPIPE from becoming an uncaught exception;
      // writeStdin installs a scoped listener when it is waiting for drain.
      child.stdin.on('error', () => {});
      const job: ProcessJob = {
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
      job.task = this.watch(job, timeout_seconds).finally(() => {
        job.closed = true;
      });
      // The creation lock must not serialize the caller's optional wait.
      release.value!();
      release.value = null;
      if (wait_seconds) {
        await Promise.race([job.task, sleep(wait_seconds * 1000)]);
      }
      return this.read(jobId);
    } finally {
      if (release.value) {
        release.value!();
      }
    }
  }

  // A transient gateway outage must not immediately kill local jobs, but an
  // agent that stays offline forever must not leave them running forever.
  markOffline(graceMs: number = 5 * 60 * 1000) {
    if (this.offlineTimer) {
      return;
    }
    this.accepting = false;
    this.offlineTimer = setTimeout(() => {
      this.offlineTimer = null;
      void this.close();
    }, graceMs);
    this.offlineTimer.unref?.();
  }

  markOnline() {
    if (this.offlineTimer) {
      clearTimeout(this.offlineTimer);
    }
    this.offlineTimer = null;
    this.accepting = true;
  }

  append(job: ProcessJob, text: string) {
    if (!text) {
      return;
    }
    const length = codePoints(text).length;
    job.segments.push({ text, length });
    job.output_length += length;
    while (job.output_length > MAX_OUTPUT && job.segments.length) {
      const excess = job.output_length - MAX_OUTPUT;
      const first = job.segments[0]!;
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

  async watch(job: ProcessJob, timeoutSeconds: number) {
    const decoder = new StringDecoder('utf8');
    job.process.stdout.on('data', (chunk: Buffer) => this.append(job, decoder.write(chunk)));
    const closed = new Promise<number | null>((resolve) => {
      job.process.once('close', resolve);
      job.process.once('error', () => resolve(null));
    });
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      const result = await Promise.race([
        closed.then((code: number | null) => ({ kind: 'closed' as const, code })),
        new Promise<{ kind: 'timeout' }>((resolve) => {
          timer = setTimeout(() => resolve({ kind: 'timeout' }), timeoutSeconds * 1000);
        }),
      ]);
      if (result.kind === 'closed') {
        if (job.status === 'running') {
          job.status = 'exited';
        }
      } else {
        job.status = 'timed_out';
      }
    } catch (error) {
      if (job.status === 'running') {
        job.status = 'terminated';
      }
      throw error;
    } finally {
      clearTimeout(timer);
      this.append(job, decoder.end());
      await this.kill(job);
      await closed;
    }
  }

  async kill(job: ProcessJob) {
    if (!job.process.pid) {
      return;
    }
    if (process.platform === 'win32') {
      // taskkill handles a live command's ordinary descendants. Windows does
      // not provide POSIX process groups; detached descendants after the parent
      // exits require a future Job Object adapter and are not guaranteed here.
      const executable = `${process.env.SystemRoot || 'C:\\Windows'}\\System32\\taskkill.exe`;
      await new Promise<void>((resolve) => {
        const killer = spawn(executable, ['/pid', String(job.process.pid), '/T', '/F'], {
          windowsHide: true,
          stdio: 'ignore',
        });
        const timer = setTimeout(() => {
          killer.kill();
          resolve();
        }, 5000);
        const done = () => {
          clearTimeout(timer);
          resolve();
        };
        killer.once('error', done);
        killer.once('close', done);
      });
      return;
    }
    try {
      process.kill(-job.process.pid, 'SIGKILL');
    } catch (error) {
      if (errorCode(error) !== 'ESRCH') {
        try {
          job.process.kill('SIGKILL');
        } catch {}
      }
    }
  }

  read(job_id: unknown, offset: unknown = 0, limit: unknown = 32768) {
    if (typeof job_id !== 'string' || !this.jobs.has(job_id)) {
      throw new Error('Unknown process job');
    }
    if (!isInteger(offset) || offset < 0 || !isInteger(limit) || limit < 1 || limit > 65536) {
      throw new Error('Invalid offset/limit');
    }
    const job = this.jobs.get(job_id)!;
    const start = Math.max(offset, job.base_offset);
    let relative = start - job.base_offset;
    let remaining = limit;
    const output = [];
    for (const segment of job.segments) {
      if (relative >= segment.length) {
        relative -= segment.length;
        continue;
      }
      const take = Math.min(remaining, segment.length - relative);
      output.push(sliceCodePoints(segment.text, relative, relative + take));
      remaining -= take;
      relative = 0;
      if (!remaining) {
        break;
      }
    }
    const content = output.join('');
    return {
      job_id,
      pid: job.process.pid,
      status: job.status,
      exit_code: job.process.exitCode,
      cleanup_pending: job.status !== 'running' && !job.closed,
      output: content,
      offset: start,
      next_offset: start + codePoints(content).length,
      output_dropped: offset < job.base_offset,
      started_at: job.started,
    };
  }

  list() {
    return {
      jobs: [...this.jobs.entries()].map(([job_id, job]) => ({
        job_id,
        pid: job.process.pid,
        status: job.status,
        exit_code: job.process.exitCode,
        started_at: job.started,
        cleanup_pending: job.status !== 'running' && !job.closed,
      })),
    };
  }

  async writeStdin(job_id: unknown, text: unknown = '', close: unknown = false) {
    const job = typeof job_id === 'string' ? this.jobs.get(job_id) : undefined;
    if (
      !job ||
      job.status !== 'running' ||
      typeof text !== 'string' ||
      typeof close !== 'boolean' ||
      Buffer.byteLength(text) > 16384
    ) {
      throw new Error('Process is not running or input exceeds 16 KiB');
    }
    const release: { value: (() => void) | null } = { value: null };
    const previous = job.stdinLock;
    job.stdinLock = new Promise<void>((resolve) => {
      release.value = resolve;
    });
    await previous;
    try {
      if (job.status !== 'running') {
        throw new Error('Process is not running or input exceeds 16 KiB');
      }
      if (text && !job.process.stdin.write(text)) {
        await new Promise<void>((resolve, reject) => {
          let timer: ReturnType<typeof setTimeout> | undefined;
          const done = (error?: unknown) => {
            clearTimeout(timer);
            job.process.stdin.removeListener('drain', onDrain);
            job.process.stdin.removeListener('error', onError);
            job.process.removeListener('close', onClose);
            if (error) {
              reject(error);
            } else {
              resolve();
            }
          };
          const onDrain = () => done();
          const onError = (error: Error) => done(error);
          const onClose = () => done(new Error('Process is not running or input exceeds 16 KiB'));
          timer = setTimeout(
            () => done(new Error('Process stdin did not drain within 3 seconds')),
            3000,
          );
          job.process.stdin.once('drain', onDrain);
          job.process.stdin.once('error', onError);
          job.process.once('close', onClose);
        });
      }
      if (close) {
        job.process.stdin.end();
      }
    } finally {
      release.value!();
    }
    return { job_id, sent: codePoints(text).length, stdin_closed: close };
  }

  async terminate(job_id: unknown) {
    const job = typeof job_id === 'string' ? this.jobs.get(job_id) : undefined;
    if (!job) {
      throw new Error('Unknown process job');
    }
    if (!job.closed) {
      if (job.status === 'running') {
        job.status = 'terminated';
      }
      await this.kill(job);
      await waitBounded(job.task, CLOSE_WAIT_MS);
    }
    return this.read(job_id);
  }

  async close() {
    this.accepting = false;
    const pendingStart = this.startTail;
    const stopping = [...this.jobs.values()]
      .filter((job) => !job.closed)
      .map(async (job: ProcessJob) => {
        if (job.status === 'running') {
          job.status = 'terminated';
        }
        await this.kill(job);
        return waitBounded(job.task, CLOSE_WAIT_MS);
      });
    if (this.offlineTimer) {
      clearTimeout(this.offlineTimer);
    }
    this.offlineTimer = null;
    await Promise.all([...stopping, waitBounded(pendingStart, START_WAIT_MS)]);
  }
}
