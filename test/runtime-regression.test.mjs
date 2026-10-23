import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { Agent } from '../.build/agent.js';
import { AuditLog } from '../.build/audit.js';
import { AgentConfig } from '../.build/config.js';
import { Files } from '../.build/files.js';
import { buildHardening, hardeningPath, validateHardening } from '../.build/hardening.js';
import { ensurePrivatePath, secureNewPath } from '../.build/platform.js';
import { processEnvironment } from '../.build/process-environment.js';
import { Processes } from '../.build/processes.js';
import { Policy } from '../.build/policy.js';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(fs.realpathSync.native(os.tmpdir()), 'rdh-runtime-'));
  secureNewPath(root, true);
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const config = AgentConfig.validate({
    gateway_url: 'http://127.0.0.1:9',
    device_id: 'runtime-device',
    token: 't'.repeat(48),
    roots: { work: root },
    allow_shell: true,
    allow_write: true,
    max_process_seconds: 5,
  });
  const files = new Files(config.roots, true);
  return { root, config, files };
}

function journal(t, root, required = false) {
  const log = new AuditLog({ directory: path.join(root, 'audit'), required });
  t.after(() => log.close());
  return log;
}

const nodeCommand = (source) =>
  `"${process.execPath}" -e "eval(Buffer.from('${Buffer.from(source).toString('base64')}','base64').toString())"`;
const entries = (log) =>
  fs.readFileSync(log.file, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);

test('spawn failures reject and remain inspectable as failed jobs without command arguments', async (t) => {
  const { root, config, files } = fixture(t);
  config.shell = path.join(root, process.platform === 'win32' ? 'cmd.exe' : 'missing-shell');
  const processes = new Processes(config, files);
  t.after(() => processes.close());
  await assert.rejects(processes.start('private-command-text', 'work', '.', 3, 0), (error) => {
    assert.equal(error.code, 'process_start_failed');
    assert.equal(error.details.code, 'ENOENT');
    assert.equal(typeof error.details.errno, 'number');
    assert.match(error.details.syscall, /spawn/);
    assert.equal(error.details.path, config.shell);
    assert.ok(!error.message.includes('private-command-text'));
    const failed = processes.read(error.jobId);
    assert.equal(failed.status, 'failed');
    assert.equal(failed.error.code, 'ENOENT');
    return true;
  });
  await processes.close();
  const [failed] = processes.list().jobs;
  assert.equal(failed.status, 'failed');
  assert.equal(failed.cleanup_pending, false);
  assert.equal(failed.error.code, 'ENOENT');
});

test('captures stderr from early command statements and keeps later stdout', async (t) => {
  const { config, files } = fixture(t);
  const processes = new Processes(config, files);
  t.after(() => processes.close());
  const separator = process.platform === 'win32' ? ' & ' : '; ';
  const command =
    nodeCommand("process.stderr.write('early diagnostic'); process.exitCode = 7") +
    separator +
    nodeCommand("process.stdout.write('later output')");
  const result = await processes.start(command, 'work', '.', 5, 3);
  assert.equal(result.status, 'exited');
  assert.match(result.output, /early diagnostic/);
  assert.match(result.output, /later output/);
});

test('the omitted process timeout respects a lower configured maximum', async (t) => {
  const { config, files } = fixture(t);
  config.max_process_seconds = 1;
  const processes = new Processes(config, files);
  t.after(() => processes.close());
  const result = await processes.start(nodeCommand("process.stdout.write('short task')"), 'work');
  assert.equal(result.status, 'exited');
  assert.equal(result.output, 'short task');
});

test('child environments preserve session essentials and explicitly inherited application values', () => {
  const parent = {
    PATH: '/usr/bin',
    XDG_RUNTIME_DIR: '/run/user/2000',
    DBUS_SESSION_BUS_ADDRESS: 'unix:path=/run/user/2000/bus',
    RDH_BUILD_MODE: 'preview',
    PRIVATE_VALUE: 'hidden',
  };
  const env = processEnvironment({ parent, platform: 'linux', inherit: ['RDH_BUILD_MODE'] });
  assert.equal(env.XDG_RUNTIME_DIR, parent.XDG_RUNTIME_DIR);
  assert.equal(env.DBUS_SESSION_BUS_ADDRESS, parent.DBUS_SESSION_BUS_ADDRESS);
  assert.equal(env.RDH_BUILD_MODE, 'preview');
  assert.equal(env.PRIVATE_VALUE, undefined);
  const isolated = processEnvironment({ parent, platform: 'linux', session: false });
  assert.equal(isolated.XDG_RUNTIME_DIR, undefined);
  assert.equal(isolated.DBUS_SESSION_BUS_ADDRESS, undefined);
  const windows = processEnvironment({
    platform: 'win32',
    parent: {
      path: 'C:\\Tools',
      systemroot: 'C:\\Windows',
      pathext: '.CUSTOM;.exe',
      username: 'builder',
      computername: 'test-host',
      sessionname: 'test-session',
      userdomain: 'test-domain',
    },
  });
  assert.equal(windows.PATH, 'C:\\Tools');
  assert.equal(windows.WINDIR, 'C:\\Windows');
  assert.equal(windows.COMPUTERNAME, 'test-host');
  assert.equal(windows.USERDOMAIN, 'test-domain');
  assert.match(windows.PATHEXT, /\.CUSTOM/);
  assert.equal(
    windows.PATHEXT.toUpperCase()
      .split(';')
      .filter((value) => value === '.EXE').length,
    1,
  );
});

test('sidecar environment names are validated and reach a real child', async (t) => {
  const { root, config } = fixture(t);
  const previous = process.env.RDH_BUILD_MODE;
  process.env.RDH_BUILD_MODE = 'acceptance';
  t.after(() => {
    if (previous === undefined) {
      delete process.env.RDH_BUILD_MODE;
    } else {
      process.env.RDH_BUILD_MODE = previous;
    }
  });
  for (const inherit_env of ['name', ['BAD=NAME'], [''], Array(65).fill('RDH_BUILD_MODE')]) {
    assert.throws(() => validateHardening({ version: 1, inherit_env }), /inherit_env/);
  }
  const file = path.join(root, 'device.json');
  ensurePrivatePath(hardeningPath(file), false);
  fs.writeFileSync(
    hardeningPath(file),
    JSON.stringify({ version: 1, inherit_env: ['RDH_BUILD_MODE'] }),
  );
  const options = buildHardening(config, file, {
    env: { XDG_STATE_HOME: root, LOCALAPPDATA: root },
  });
  const agent = new Agent(config, options);
  t.after(() => agent.close());
  const result = await agent.dispatch('start_process', {
    root: 'work',
    command: nodeCommand('process.stdout.write(process.env.RDH_BUILD_MODE)'),
    wait_seconds: 3,
  });
  assert.equal(result.output, 'acceptance');
  assert.deepEqual(agent.info().hardening.inherit_env, ['RDH_BUILD_MODE']);
});

test('UTF-8 text pagination preserves full characters and rejects a partial start', async (t) => {
  const { root, files } = fixture(t);
  const raw = Buffer.from('go🐢高!');
  fs.writeFileSync(path.join(root, 'text.txt'), raw);
  let offset = 0;
  const text = [];
  const bytes = [];
  do {
    const result = await files.read({ root: 'work', path: 'text.txt', offset, length: 5 });
    assert.ok(result.next_offset > offset);
    text.push(result.content);
    bytes.push(Buffer.from(result.content_base64, 'base64'));
    offset = result.next_offset;
  } while (offset < raw.length);
  assert.equal(text.join(''), raw.toString('utf8'));
  assert.deepEqual(Buffer.concat(bytes), raw);
  await assert.rejects(
    files.read({ root: 'work', path: 'text.txt', offset: 3, length: 5 }),
    /align|characters/,
  );
  await assert.rejects(
    files.read({ root: 'work', path: 'text.txt', offset: 2, length: 1 }),
    /characters/,
  );
});

test('BOM-tagged UTF-16 reads preserve characters, bytes and source offsets', async (t) => {
  const { root, files } = fixture(t);
  for (const encoding of ['utf-16le', 'utf-16be']) {
    const raw = Buffer.from('\uFEFFCalm🪁\r\n', 'utf16le');
    if (encoding === 'utf-16be') {
      raw.swap16();
    }
    const filename = `${encoding}.txt`;
    fs.writeFileSync(path.join(root, filename), raw);
    let offset = 0;
    let text = '';
    const bytes = [];
    while (offset < raw.length) {
      const result = await files.read({ root: 'work', path: filename, offset, length: 7 });
      assert.equal(result.content_encoding, encoding);
      assert.ok(result.next_offset > offset);
      text += result.content;
      bytes.push(Buffer.from(result.content_base64, 'base64'));
      offset = result.next_offset;
    }
    assert.equal(text, '\uFEFFCalm🪁\r\n');
    assert.deepEqual(Buffer.concat(bytes), raw);
    await assert.rejects(files.read({ root: 'work', path: filename, offset: 1 }), /aligned/);
    assert.equal(
      (await files.readFileChunk({ root: 'work', path: filename })).content_base64,
      raw.toString('base64'),
    );
  }
});

test('multi-file defaults work and decoded budgets retain UTF-16 source offsets', async (t) => {
  const { root, files } = fixture(t);
  const raw = Buffer.from('\uFEFFDog🐬', 'utf16le');
  fs.writeFileSync(path.join(root, 'note.txt'), raw);
  const defaultRead = await files.readMultipleFiles({ root: 'work', paths: ['note.txt'] });
  assert.equal(defaultRead.files[0].content, '\uFEFFDog🐬');
  const bounded = await files.readMultipleFiles({
    root: 'work',
    paths: ['note.txt'],
    total_limit: 5,
  });
  assert.equal(bounded.files[0].content, '\uFEFFDo');
  assert.equal(bounded.files[0].next_offset, 6);
  assert.equal(bounded.total_bytes, 5);
  assert.equal(bounded.files[0].content_base64, raw.subarray(0, 6).toString('base64'));
});

test('UTF-8 edits preserve an existing BOM and reject binary text reads', async (t) => {
  const { root, files } = fixture(t);
  fs.writeFileSync(path.join(root, 'bom.txt'), '\uFEFFold value');
  const before = await files.read({ root: 'work', path: 'bom.txt' });
  await files.edit({
    root: 'work',
    path: 'bom.txt',
    old_text: 'old',
    new_text: 'new',
    expected_sha256: before.sha256,
  });
  assert.equal(fs.readFileSync(path.join(root, 'bom.txt'), 'utf8'), '\uFEFFnew value');
  fs.writeFileSync(path.join(root, 'binary.dat'), Buffer.from([0xff, 0x42, 0x80]));
  await assert.rejects(files.read({ root: 'work', path: 'binary.dat' }), /not valid UTF-8/);
  assert.equal(
    (await files.readFileChunk({ root: 'work', path: 'binary.dat' })).content_base64,
    '/0KA',
  );
});

test('recursive directories and new-file parents stay within validated roots', async (t) => {
  const { root, files, config } = fixture(t);
  await files.mkdir({ root: 'work', path: 'nested/path/leaf' });
  await files.mkdir({ root: 'work', path: 'nested/path/leaf' });
  await files.write({ root: 'work', path: 'new/tree/note.txt', content: 'created' });
  assert.equal(fs.readFileSync(path.join(root, 'new/tree/note.txt'), 'utf8'), 'created');
  await assert.rejects(
    files.write({
      root: 'work',
      path: 'absent/tree/note.txt',
      content: 'no update',
      expected_sha256: 'expected',
    }),
  );
  assert.equal(fs.existsSync(path.join(root, 'absent')), false);
  const outside = fs.mkdtempSync(path.join(fs.realpathSync.native(os.tmpdir()), 'rdh-outside-'));
  t.after(() => fs.rmSync(outside, { recursive: true, force: true }));
  fs.symlinkSync(
    outside,
    path.join(root, 'link'),
    process.platform === 'win32' ? 'junction' : 'dir',
  );
  await assert.rejects(files.write({ root: 'work', path: 'link/new/note.txt', content: 'escape' }));
  assert.equal(fs.existsSync(path.join(outside, 'new')), false);
  const agent = new Agent(config, { policy: new Policy({ protected_paths: ['blocked'] }) });
  t.after(() => agent.close());
  await assert.rejects(
    agent.dispatch('write_file', { root: 'work', path: 'blocked/new/note.txt', content: 'denied' }),
    /protected/,
  );
  assert.equal(fs.existsSync(path.join(root, 'blocked')), false);
  await assert.rejects(
    new Files(config.roots, false).mkdir({ root: 'work', path: 'readonly/new' }),
    /disabled/,
  );
});

test('required audit failure blocks effects while health queries remain available', async (t) => {
  const { root, config } = fixture(t);
  const log = journal(t, root, true);
  const agent = new Agent(config, { audit: log });
  t.after(() => agent.close());
  const original = fs.writeSync;
  t.mock.method(fs, 'writeSync', (fd, ...args) => {
    if (fd === log.fd) {
      throw Object.assign(new Error('storage unavailable'), { code: 'ENOSPC' });
    }
    return original(fd, ...args);
  });
  await assert.rejects(
    agent.dispatch('write_file', { root: 'work', path: 'not-created.txt', content: 'blocked' }),
    { code: 'audit_write_failed' },
  );
  assert.equal(fs.existsSync(path.join(root, 'not-created.txt')), false);
  const health = await agent.dispatch('device_info', {});
  assert.equal(health.hardening.local_audit.required, true);
  assert.ok(health.hardening.local_audit.dropped >= 2);
  assert.equal(health.hardening.local_audit.last_error, 'ENOSPC');
});

test('failed audit synchronization blocks effects and removes the uncommitted record', async (t) => {
  const { root, config } = fixture(t);
  const log = journal(t, root, true);
  const agent = new Agent(config, { audit: log });
  t.after(() => agent.close());
  const original = fs.fsyncSync;
  const mock = t.mock.method(fs, 'fsyncSync', (fd) => {
    if (fd === log.fd) {
      throw Object.assign(new Error('storage synchronization failed'), { code: 'EIO' });
    }
    return original(fd);
  });
  const params = { root: 'work', path: 'after-sync.txt', content: 'recorded' };
  await assert.rejects(agent.dispatch('write_file', params), { code: 'audit_write_failed' });
  assert.equal(fs.existsSync(path.join(root, params.path)), false);
  assert.equal(fs.readFileSync(log.file, 'utf8'), '');
  assert.equal(log.stats().dropped, 1);
  assert.equal(log.stats().last_error, 'EIO');
  mock.mock.restore();
  await agent.dispatch('write_file', params);
  assert.equal(fs.readFileSync(path.join(root, params.path), 'utf8'), params.content);
  assert.deepEqual(
    entries(log).map((entry) => entry.status),
    ['started', 'ok'],
  );
  assert.equal(log.stats().last_error, null);
});

test('failed outcome auditing reports completed effects without retrying the operation', async (t) => {
  const { root, config } = fixture(t);
  const log = journal(t, root, true);
  const agent = new Agent(config, { audit: log });
  t.after(() => agent.close());
  const original = fs.writeSync;
  let writes = 0;
  const mock = t.mock.method(fs, 'writeSync', (fd, ...args) => {
    if (fd === log.fd && ++writes === 2) {
      throw Object.assign(new Error('storage unavailable'), { code: 'ENOSPC' });
    }
    return original(fd, ...args);
  });
  await assert.rejects(
    agent.dispatch('write_file', { root: 'work', path: 'completed.txt', content: 'once' }),
    /Operation completed.*inspect before retrying/,
  );
  assert.equal(fs.readFileSync(path.join(root, 'completed.txt'), 'utf8'), 'once');
  assert.deepEqual(
    entries(log).map((entry) => entry.status),
    ['started'],
  );
  mock.mock.restore();
  await agent.dispatch('device_info', {});
  assert.equal(log.stats().last_error, null);
});

test('audit health is visible without changing the policy revision and process completion is correlated', async (t) => {
  const { root, config } = fixture(t);
  const log = journal(t, root);
  const agent = new Agent(config, { audit: log });
  t.after(() => agent.close());
  const revision = agent.info().hardening.policy_revision;
  const result = await agent.dispatch(
    'start_process',
    { root: 'work', command: nodeCommand('process.exitCode = 4'), wait_seconds: 2 },
    { requestId: 'completion-check' },
  );
  await agent.processes.close();
  const ended = entries(log).find((entry) => entry.op === 'process_exit');
  assert.equal(ended.id, 'completion-check');
  assert.equal(ended.status, 'exited');
  assert.equal(ended.result.job_id, result.job_id);
  assert.equal(ended.result.exit_code, 4);
  assert.equal(agent.info().hardening.policy_revision, revision);
  assert.ok(agent.info().hardening.local_audit.written >= 2);
});
