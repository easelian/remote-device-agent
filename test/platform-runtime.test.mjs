import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { AgentConfig, readConfig, writeConfig } from '../.build/config.js';
import { Files } from '../.build/files.js';
import { Processes } from '../.build/processes.js';
import { Agent } from '../.build/agent.js';
import { acquireProfileLocks } from '../.build/profile-lock.js';
import { pairDevice } from '../.build/connect.js';
import { defaultShell, ensurePrivatePath, platformName } from '../.build/platform.js';

async function fixture(t) {
  const directory = await fs.realpath(await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-platform-')));
  t.after(() => fs.rm(directory, { recursive: true, force: true }));
  const work = path.join(directory, 'work');
  await fs.mkdir(work);
  const config = AgentConfig.validate({
    gateway_url: 'http://127.0.0.1:8765',
    device_id: 'native-test',
    token: 't'.repeat(40),
    roots: { work },
    allow_shell: true,
    allow_write: true,
  });
  return { directory, work, config };
}

const nodeCommand = (code) =>
  `"${process.execPath}" -e "eval(Buffer.from('${Buffer.from(code).toString('base64')}','base64').toString())"`;

test('native platform configuration, private credentials and exclusive profile locks', async (t) => {
  const { directory, config } = await fixture(t);
  assert.equal(config.shell, defaultShell());
  const state = path.join(directory, 'state');
  ensurePrivatePath(state, true);
  const filename = path.join(state, 'device.json');
  await writeConfig(filename, config);
  assert.deepEqual(await readConfig(filename), config);
  const first = await acquireProfileLocks([filename]);
  try {
    await assert.rejects(acquireProfileLocks([filename]), /lock|manual cleanup/);
  } finally {
    await first.release();
  }
  const second = await acquireProfileLocks([filename]);
  await second.release();
  await assert.rejects(fs.stat(`${filename}.node-lock`), { code: 'ENOENT' });
});

test('native filesystem writes, chunk reads, moves and junction rejection', async (t) => {
  const { directory, work, config } = await fixture(t);
  const files = new Files(config.roots, true);
  await files.mkdir({ root: 'work', path: 'nested' });
  await files.write({
    root: 'work',
    path: 'nested/a.txt',
    content: 'hello😀',
    expected_sha256: '',
  });
  assert.equal((await files.read({ root: 'work', path: 'nested/a.txt' })).content, 'hello😀');
  assert.equal(
    (await files.readFileChunk({ root: 'work', path: 'nested/a.txt', offset: 0, length: 5 }))
      .content,
    'hello',
  );
  await files.moveFile({ root: 'work', source: 'nested/a.txt', destination: 'nested/b.txt' });
  await assert.rejects(fs.stat(path.join(work, 'nested/a.txt')), { code: 'ENOENT' });
  assert.equal(await fs.readFile(path.join(work, 'nested/b.txt'), 'utf8'), 'hello😀');
  const outside = path.join(directory, 'outside');
  await fs.mkdir(outside);
  await fs.writeFile(path.join(outside, 'secret.txt'), 'not exposed');
  await fs.symlink(
    outside,
    path.join(work, 'link'),
    process.platform === 'win32' ? 'junction' : 'dir',
  );
  await assert.rejects(files.read({ root: 'work', path: 'link/secret.txt' }));
  if (process.platform === 'win32') {
    for (const name of [
      'C:/escape',
      'file:stream',
      'NUL.txt',
      'nested\\..\\outside',
      'nested/b.txt.',
    ]) {
      await assert.rejects(files.read({ root: 'work', path: name }));
    }
  }
});

test('native shell output, stdin, timeout and supervised process cleanup', async (t) => {
  const { config } = await fixture(t);
  const files = new Files(config.roots, true);
  const processes = new Processes(config, files);
  t.after(() => processes.close());
  const output = await processes.start(
    nodeCommand("process.stdout.write('native😀'); process.stderr.write('stderr');"),
    'work',
    '.',
    10,
    1,
  );
  assert.match(output.output, /native😀/);
  assert.match(output.output, /stderr/);
  const input = await processes.start(
    nodeCommand('process.stdin.pipe(process.stdout)'),
    'work',
    '.',
    10,
    0,
  );
  await processes.writeStdin(input.job_id, 'stdin😀', true);
  for (
    let i = 0;
    i < 100 &&
    (processes.read(input.job_id).cleanup_pending ||
      processes.read(input.job_id).status === 'running');
    i++
  ) {
    await new Promise((resolve) => setTimeout(resolve, 30));
  }
  assert.notEqual(processes.read(input.job_id).status, 'running');
  assert.equal(processes.read(input.job_id).output, 'stdin😀');
  const timed = await processes.start(
    nodeCommand('setInterval(() => {}, 1000)'),
    'work',
    '.',
    1,
    3,
  );
  assert.equal(timed.status, 'timed_out');
  await processes.close();
  assert.ok(processes.list().jobs.every((job) => !job.cleanup_pending));
});

test('pairing advertises native OS and preserves local roots and default shell', async (t) => {
  const { config } = await fixture(t);
  let proposal;
  t.mock.method(globalThis, 'fetch', async (url, options) => {
    if (new URL(url).pathname === '/api/pair/start') {
      proposal = JSON.parse(options.body);
      return Response.json(
        {
          device_code: 'code',
          user_code: 'ABCDEFGH',
          interval: 0,
          expires_in: 10,
          verification_uri_complete: 'http://127.0.0.1:8765/admin/pair?code=ABCDEFGH',
        },
        { status: 201 },
      );
    }
    return Response.json({ config: { ...config, shell: '/bin/sh' } });
  });
  const paired = await pairDevice(config.gateway_url, config.roots, 'native device', {
    openBrowser: false,
  });
  assert.equal(proposal.os, platformName());
  assert.deepEqual(paired.roots, config.roots);
  assert.equal(paired.shell, defaultShell());
  const agent = new Agent(config);
  assert.equal(agent.info().os, platformName());
  assert.equal(
    agent.info().capabilities.process_tree_termination,
    process.platform === 'win32' ? 'taskkill-best-effort' : 'process-group',
  );
  await agent.close();
});
