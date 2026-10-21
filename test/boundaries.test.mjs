import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { AgentConfig } from '../.build/config.js';
import { pairDevice, connectDevice } from '../.build/connect.js';
import { Files } from '../.build/files.js';
import { validateHardening } from '../.build/hardening.js';
import native from '../.build/posix-fs.js';
import { acquireProfileLocks, reclaimProfileLocks } from '../.build/profile-lock.js';
import { Processes } from '../.build/processes.js';

function temporary(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'rdh-boundaries-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  return root;
}

function configuration(root) {
  return AgentConfig.validate({
    gateway_url: 'https://example.test',
    device_id: 'boundaries',
    token: 't'.repeat(48),
    roots: { work: root },
    allow_shell: true,
    allow_write: true,
  });
}

test('config JSON validates field types and retains defaults and named roots', (t) => {
  const root = temporary(t);
  const config = configuration(root);
  assert.equal(config.max_processes, 4);
  assert.equal(config.max_process_seconds, 3600);
  for (const input of [
    null,
    [],
    { ...config, roots: [] },
    { ...config, roots: { work: 17 } },
    { ...config, max_processes: '4' },
    { ...config, allow_write: 1 },
    { ...config, shell: {} },
  ]) {
    assert.throws(() => AgentConfig.validate(input), /设备配置|设备目录/);
  }
  const specialRoots = JSON.parse(JSON.stringify({ ...config, roots: { ['__proto__']: root } }));
  const validated = AgentConfig.validate(specialRoots);
  assert.equal(Object.hasOwn(validated.roots, '__proto__'), true);
  assert.equal(validated.roots.__proto__, root);
  assert.equal(Object.getPrototypeOf(validated.roots), Object.prototype);
});

test('malformed pairing JSON is rejected before browser launch or polling', async (t) => {
  const original = globalThis.fetch;
  t.after(() => {
    globalThis.fetch = original;
  });
  const pending = {
    verification_uri_complete: 'https://example.test/admin/pair?code=TEST',
    user_code: 'TEST',
    device_code: 'pending',
    expires_in: 600,
    interval: 3,
  };
  for (const value of [
    null,
    [],
    { ...pending, user_code: {} },
    { ...pending, device_code: 17 },
    { ...pending, expires_in: {} },
    { ...pending, interval: [] },
  ]) {
    let requests = 0;
    globalThis.fetch = async () => {
      requests++;
      return Response.json(value, { status: 201 });
    };
    await assert.rejects(
      () =>
        pairDevice('https://example.test', { work: os.tmpdir() }, 'device', { openBrowser: false }),
      /无效的绑定/,
    );
    assert.equal(requests, 1);
  }
});

test('hardening JSON rejects malformed policy and limit fields without weakening defaults', () => {
  for (const field of ['allowed_operations', 'readonly_roots', 'protected_paths']) {
    for (const value of ['work', [17], {}]) {
      assert.throws(() => validateHardening({ version: 1, [field]: value }, ['work']), /array/);
    }
  }
  for (const limits of [[], { cpu_seconds: '1' }, JSON.parse('{"__proto__":1}')]) {
    assert.throws(
      () => validateHardening({ version: 1, sandbox: { mode: 'none', limits } }),
      /limits/,
    );
  }
  assert.throws(
    () => validateHardening({ version: 1, audit: { enabled: true, max_bytes: '4096' } }),
    /audit/,
  );
  assert.deepEqual(validateHardening({ version: 1 }).readonlyRoots, []);
  assert.equal(validateHardening({ version: 1 }).sandbox, null);
});

test('bounded filesystem adapters support synchronous and asynchronous operations in both call formats', async (t) => {
  const root = temporary(t);
  fs.writeFileSync(path.join(root, 'sample.txt'), 'abc');
  for (const asynchronous of [false, true]) {
    const openHandles = new Set();
    const adapter = {
      ...native,
      openDirectory: async (...args) => {
        const handle = await native.openDirectory(...args);
        openHandles.add(handle);
        return handle;
      },
      close: async (handle) => {
        await native.close(handle);
        openHandles.delete(handle);
      },
      readFile: () => (asynchronous ? Promise.resolve(Buffer.from('abc')) : Buffer.from('abc')),
      listDirectory: () => {
        const entries = [{ name: 'sample.txt', type: 'file', size_bytes: 3 }];
        return asynchronous ? Promise.resolve(entries) : entries;
      },
    };
    const files = new Files({ work: root }, true, adapter);
    assert.deepEqual(
      await files.read('work', 'sample.txt'),
      await files.read({ root: 'work', path: 'sample.txt' }),
    );
    assert.deepEqual(await files.list('work'), await files.list({ root: 'work' }));
    assert.equal(openHandles.size, 0);
    adapter.readFileChunk = () => {
      throw { code: 'EIO', message: 'injected read failure' };
    };
    const result = await files.readMultipleFiles({
      files: [null, { root: 'work', path: 'sample.txt' }],
    });
    assert.equal(result.files[0].error, 'Invalid file request');
    assert.equal(result.files[1].error, 'injected read failure');
    assert.equal(openHandles.size, 0);
  }
});

test('file edits reject non-string replacements before modifying bytes', async (t) => {
  const root = temporary(t);
  const target = path.join(root, 'sample.txt');
  fs.writeFileSync(target, 'before');
  const files = new Files({ work: root }, true);
  const previous = await files.read('work', 'sample.txt');
  await assert.rejects(
    () =>
      files.edit({
        root: 'work',
        path: 'sample.txt',
        old_text: 'before',
        new_text: { value: 'after' },
        expected_sha256: previous.sha256,
      }),
    /must be strings/,
  );
  assert.equal(fs.readFileSync(target, 'utf8'), 'before');
});

test('malformed lock owners cannot be reclaimed or released', async (t) => {
  const root = temporary(t);
  const config = path.join(root, 'profile.json');
  const lock = `${config}.node-lock`;
  for (const owner of [
    null,
    [],
    { pid: '999999999', nonce: 'x'.repeat(43) },
    { pid: 999999999, nonce: [] },
  ]) {
    fs.mkdirSync(lock, { mode: 0o700 });
    fs.writeFileSync(path.join(lock, 'owner.json'), JSON.stringify(owner), { mode: 0o600 });
    await assert.rejects(() => reclaimProfileLocks([config]), /配置锁已存在/);
    assert.equal(fs.statSync(lock).isDirectory(), true);
    fs.rmSync(lock, { recursive: true });
  }
  const handle = await acquireProfileLocks([config]);
  fs.writeFileSync(path.join(lock, 'owner.json'), 'null', { mode: 0o600 });
  await handle.release();
  assert.equal(fs.statSync(lock).isDirectory(), true);
});

test('legacy lock providers release their exact opaque handle after an injected runner failure', async (t) => {
  const root = temporary(t);
  const configFile = path.join(root, 'profile.json');
  fs.writeFileSync(configFile, JSON.stringify(configuration(root)), { mode: 0o600 });
  const handle = Symbol('lock-owner');
  const calls = [];
  await assert.rejects(
    () =>
      connectDevice(
        { server: 'https://example.test', config: configFile },
        {
          locks: {
            acquire: async () => {
              calls.push('acquire');
              return handle;
            },
            release: async (value) => {
              assert.equal(value, handle);
              calls.push('release');
            },
          },
          runAgent: async () => {
            calls.push('run');
            throw new Error('runner failed');
          },
        },
      ),
    /runner failed/,
  );
  assert.deepEqual(calls, ['acquire', 'run', 'release']);
});

test('malformed stdin close flag cannot send bytes or close a running job', {
  skip: process.platform === 'win32',
}, async (t) => {
  const root = temporary(t);
  const processes = new Processes(configuration(root), new Files({ work: root }, true));
  try {
    const job = await processes.start('cat', 'work', '.', 5, 0);
    await assert.rejects(
      () => processes.writeStdin(job.job_id, 'discarded', 'yes'),
      /Process is not running or input exceeds/,
    );
    const result = await processes.writeStdin(job.job_id, 'accepted', true);
    assert.equal(result.stdin_closed, true);
    await processes.jobs.get(job.job_id).task;
    assert.equal(processes.read(job.job_id).output, 'accepted');
  } finally {
    await processes.close();
  }
});
