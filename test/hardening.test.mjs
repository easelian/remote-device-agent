import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { Agent } from '../lib/agent.mjs';
import { AuditLog, summarizeParams } from '../lib/audit.mjs';
import { buildHardening, hardeningPath, readSidecar, validateHardening } from '../lib/hardening.mjs';
import { Policy } from '../lib/policy.mjs';
import { buildBwrapArgs, limitsPrefix, probeBwrap } from '../lib/sandbox.mjs';

function tempDir(t) {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'rdh-hardening-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  return directory;
}

const baseConfig = (roots) => ({ gateway_url: 'http://127.0.0.1:9/', device_id: 'test-device', token: 't'.repeat(48),
  roots, allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 30 });

test('policy restricts operations, read-only roots and protected paths', () => {
  const policy = new Policy({ allowed_operations: ['read_file', 'write_file', 'edit_file', 'move_file'],
    readonly_roots: ['docs'], protected_paths: ['.git/hooks', '.env'] });
  assert.equal(policy.check('device_info', {}), null);
  assert.match(policy.check('start_process', { root: 'work', command: 'id' }), /not allowed/);
  assert.equal(policy.check('read_file', { root: 'work', path: '.env' }), null);
  assert.match(policy.check('write_file', { root: 'docs', path: 'a.txt' }), /read-only/);
  assert.match(policy.check('write_file', { root: 'work', path: 'x/.git/hooks/pre-commit' }), /protected/);
  assert.match(policy.check('edit_file', { root: 'work', path: './sub//.env' }), /protected/);
  assert.equal(policy.check('write_file', { root: 'work', path: '.git/config' }), null);
  assert.equal(policy.check('write_file', { root: 'work', path: 'hooks/.git' }), null);
  assert.match(policy.check('move_file', { root: 'work', source: 'a', destination: '.env' }), /protected/);
  assert.match(policy.check('move_file', { root: 'work', path: '.env', destination_root: 'work', destination_path: 'b' }), /protected/);
  assert.match(policy.check('move_file', { root: 'work', path: 'a', destination_root: 'docs', destination_path: 'b' }), /read-only/);
});

test('policy rejects invalid definitions', () => {
  assert.throws(() => new Policy({ allowed_operations: ['nope'] }), /Unknown operation/);
  assert.throws(() => new Policy({ protected_paths: ['/etc'] }), /Invalid protected_paths/);
  assert.throws(() => new Policy({ protected_paths: ['a/../b'] }), /Invalid protected_paths/);
  assert.throws(() => new Policy({ readonly_roots: ['bad root'] }), /Invalid root/);
});

test('audit parameter summaries are default-deny', () => {
  const summary = summarizeParams({ root: 'work', content: 'S3CRET', text: 'pw', mystery: 'LEAK', flag: true, paths: ['a', 'b'] });
  assert.deepEqual(summary.content, { bytes: 6, sha256: createHash('sha256').update('S3CRET').digest('hex') });
  assert.deepEqual(summary.mystery, { chars: 4 });
  assert.equal(summary.flag, true);
  assert.deepEqual(summary.paths, ['a', 'b']);
  assert.ok(!JSON.stringify(summary).includes('S3CRET') && !JSON.stringify(summary).includes('LEAK'));
});

test('audit log redacts content, chains records and uses private permissions', (t) => {
  const directory = path.join(tempDir(t), 'audit');
  const log = new AuditLog({ directory, name: 'dev' });
  t.after(() => log.close());
  log.record({ id: 'r1', operation: 'write_file', status: 'ok', durationMs: 3,
    params: { root: 'work', path: 'a.txt', content: 'TOP-SECRET', expected_sha256: '', unknown: 'LEAK-ME' },
    result: { size_bytes: 10, sha256: 'abc', content: 'TOP-SECRET' } });
  log.record({ id: 'r2', operation: 'start_process', status: 'error', durationMs: 1,
    params: { command: `echo ${'x'.repeat(2000)}`, root: 'work' }, error: 'boom' });
  const text = fs.readFileSync(log.file, 'utf8');
  assert.ok(!text.includes('TOP-SECRET') && !text.includes('LEAK-ME'));
  const [first, second] = text.trim().split('\n').map((line) => JSON.parse(line));
  assert.equal(first.params.content.bytes, 10);
  assert.equal(first.params.unknown.chars, 7);
  assert.equal(first.result.size_bytes, 10);
  assert.equal(first.result.content, undefined);
  assert.equal(second.params.command, undefined);
  assert.equal(second.params.command_bytes, 2005);
  assert.equal(second.params.command_chars, 2005);
  assert.equal(first.prev, '0'.repeat(16));
  assert.equal(second.prev, createHash('sha256').update(`${text.split('\n')[0]}\n`).digest('hex').slice(0, 16));
  assert.equal(fs.statSync(log.file).mode & 0o777, 0o600);
  assert.equal(fs.statSync(directory).mode & 0o777, 0o700);
});

test('audit log rotates by size and keeps a bounded number of files', (t) => {
  const directory = path.join(tempDir(t), 'audit');
  const log = new AuditLog({ directory, name: 'rot', maxBytes: 4096, maxFiles: 3 });
  t.after(() => log.close());
  for (let index = 0; index < 200; index++) log.record({ operation: 'read_file', status: 'ok', params: { root: 'work', path: `f${index}` } });
  const names = fs.readdirSync(directory).sort();
  assert.deepEqual(names, ['rot.jsonl', 'rot.jsonl.1', 'rot.jsonl.2']);
  for (const name of names) assert.ok(fs.statSync(path.join(directory, name)).size <= 4096);
  assert.equal(log.stats().dropped, 0);
});

test('audit failures never throw and are counted', (t) => {
  const log = new AuditLog({ directory: path.join(tempDir(t), 'audit'), name: 'dev' });
  fs.closeSync(log.fd);
  assert.doesNotThrow(() => log.record({ operation: 'read_file', status: 'ok' }));
  assert.equal(log.stats().dropped, 1);
  log.fd = -1;
});

test('audit log refuses a symlinked or group-readable directory', (t) => {
  const base = tempDir(t);
  const real = path.join(base, 'real');
  fs.mkdirSync(real, { mode: 0o700 });
  fs.symlinkSync(real, path.join(base, 'link'));
  assert.throws(() => new AuditLog({ directory: path.join(base, 'link') }), /real directory/);
  const open = path.join(base, 'open');
  fs.mkdirSync(open);
  fs.chmodSync(open, 0o755);
  assert.throws(() => new AuditLog({ directory: open }), /0700/);
});

test('hardening sidecar is read and validated strictly', (t) => {
  const file = path.join(tempDir(t), 'cfg.policy.json');
  assert.equal(hardeningPath('/x/abc-default.json'), '/x/abc-default.policy.json');
  assert.equal(readSidecar(file), null);
  fs.writeFileSync(file, JSON.stringify({ version: 1 }));
  fs.chmodSync(file, 0o644);
  assert.throws(() => readSidecar(file), /0600/);
  fs.chmodSync(file, 0o600);
  assert.deepEqual(readSidecar(file), { version: 1 });
  const link = `${file}.link`;
  fs.symlinkSync(file, link);
  assert.throws(() => readSidecar(link), /符号链接/);

  const roots = ['work', 'docs'];
  assert.throws(() => validateHardening({ version: 2 }, roots), /version/);
  assert.throws(() => validateHardening({ version: 1, extra: 1 }, roots), /未知字段/);
  assert.throws(() => validateHardening({ version: 1, readonly_roots: ['nope'] }, roots), /未配置/);
  assert.throws(() => validateHardening({ version: 1, sandbox: { mode: 'bwrap' } }, roots), /network/);
  assert.throws(() => validateHardening({ version: 1, sandbox: { mode: 'none', network: true } }, roots), /仅在/);
  assert.throws(() => validateHardening({ version: 1, sandbox: { mode: 'none', limits: { memory_kb: 1 } } }, roots), /limits/);
  assert.throws(() => validateHardening({ version: 1, audit: { required: true, enabled: false } }, roots), /required/);
  const ok = validateHardening({ version: 1, readonly_roots: ['docs'], protected_paths: ['.env'],
    sandbox: { mode: 'bwrap', network: false, limits: { cpu_seconds: 10 } }, audit: { max_files: 2 } }, roots);
  assert.equal(ok.sandbox.hideHome, true);
  assert.equal(ok.audit.maxFiles, 2);
  assert.ok(ok.policy);
  assert.equal(validateHardening({ version: 1 }, roots).policy, null);
});

test('bwrap arguments hide HOME before binding roots and mark read-only roots', () => {
  const args = buildBwrapArgs({ roots: { work: '/r/work', docs: '/r/docs' }, writableRoots: new Set(['work']),
    workdir: '/r/work/sub', network: false, hideHome: true, home: '/home/u', expose: ['/home/u/.nvm'],
    hiddenPaths: ['/home/u/.config/remote-device-hub'], shell: '/bin/sh', script: 'echo hi' });
  const at = (...sequence) => args.findIndex((_, index) => sequence.every((value, offset) => args[index + offset] === value));
  assert.ok(args.includes('--unshare-net') && args.includes('--die-with-parent'));
  assert.ok(at('--tmpfs', '/home/u') < at('--ro-bind', '/home/u/.nvm', '/home/u/.nvm'));
  assert.ok(at('--ro-bind', '/home/u/.nvm', '/home/u/.nvm') < at('--tmpfs', '/home/u/.config/remote-device-hub'));
  assert.ok(at('--tmpfs', '/home/u/.config/remote-device-hub') > at('--bind', '/r/work', '/r/work'));
  assert.notEqual(at('--ro-bind', '/r/docs', '/r/docs'), -1);
  assert.equal(at('--bind', '/r/docs', '/r/docs'), -1);
  assert.notEqual(at('--chdir', '/r/work/sub'), -1);
  assert.deepEqual(args.slice(-4), ['--', '/bin/sh', '-c', 'echo hi']);
  assert.ok(!buildBwrapArgs({ roots: {}, workdir: '/', network: true, shell: '/bin/sh', script: '' }).includes('--unshare-net'));
  assert.throws(() => buildBwrapArgs({ roots: {}, workdir: '/', shell: '/bin/sh', script: '' }), /network/);
});

test('resource limits become a fail-closed shell prefix', () => {
  assert.equal(limitsPrefix({}), '');
  assert.equal(limitsPrefix({ cpu_seconds: 5, open_files: 64 }), 'ulimit -t 5 || exit 126; ulimit -n 64 || exit 126; ');
  assert.throws(() => limitsPrefix({ memory_kb: 1 }), /Invalid limit/);
});

test('agent enforces policy and audits ok, error and denied operations', async (t) => {
  const base = tempDir(t);
  const root = path.join(base, 'work');
  fs.mkdirSync(root);
  const log = new AuditLog({ directory: path.join(base, 'audit'), name: 'dev' });
  const policy = new Policy({ protected_paths: ['.env'], allowed_operations: ['write_file', 'read_file'] });
  const agent = new Agent(baseConfig({ work: root }), { audit: log, policy });
  t.after(async () => { await agent.close(); log.close(); });
  await agent.dispatch('write_file', { root: 'work', path: 'a.txt', content: 'hello', expected_sha256: '' }, { requestId: 'q1' });
  await assert.rejects(agent.dispatch('write_file', { root: 'work', path: '.env', content: 'SECRET', expected_sha256: '' }), /protected/);
  await assert.rejects(agent.dispatch('read_file', { root: 'work', path: 'missing.txt' }));
  await assert.rejects(agent.dispatch('move_file', { root: 'work', source: 'a.txt', destination: 'b.txt' }), /not allowed/);
  assert.equal(fs.existsSync(path.join(root, '.env')), false);
  const lines = fs.readFileSync(log.file, 'utf8').trim().split('\n').map((line) => JSON.parse(line));
  assert.deepEqual(lines.map((line) => [line.op, line.status]),
    [['write_file', 'ok'], ['write_file', 'denied'], ['read_file', 'error'], ['move_file', 'denied']]);
  assert.equal(lines[0].id, 'q1');
  const serialized = JSON.stringify(lines);
  assert.ok(!serialized.includes('SECRET') && !serialized.includes('hello'));
});

test('agent without hardening options keeps the previous behaviour', async (t) => {
  const root = path.join(tempDir(t), 'work');
  fs.mkdirSync(root);
  const agent = new Agent(baseConfig({ work: root }));
  t.after(() => agent.close());
  const written = await agent.dispatch('write_file', { root: 'work', path: '.env', content: 'x', expected_sha256: '' });
  assert.equal(written.size_bytes, 1);
  const result = await agent.dispatch('start_process', { command: 'echo plain', root: 'work', wait_seconds: 3, timeout_seconds: 10 });
  assert.match(result.output, /plain/);
});

test('symlinks cannot be used to bypass protected paths', async (t) => {
  const root = path.join(tempDir(t), 'work');
  fs.mkdirSync(path.join(root, '.git', 'hooks'), { recursive: true });
  fs.symlinkSync(path.join(root, '.git', 'hooks'), path.join(root, 'safe'));
  const agent = new Agent(baseConfig({ work: root }), { policy: new Policy({ protected_paths: ['.git/hooks'] }) });
  t.after(() => agent.close());
  await assert.rejects(agent.dispatch('write_file', { root: 'work', path: 'safe/pre-commit', content: 'x', expected_sha256: '' }));
  assert.equal(fs.existsSync(path.join(root, '.git', 'hooks', 'pre-commit')), false);
});

const bwrapAvailable = (() => { try { probeBwrap(); return true; } catch { return false; } })();

test('bwrap sandbox hides HOME, enforces read-only roots, limits and kills through the namespace',
  { skip: !bwrapAvailable && 'bwrap unavailable' }, async (t) => {
    const base = tempDir(t);
    const home = fs.mkdtempSync(path.join(os.homedir(), '.rdh-sandbox-test-'));
    t.after(() => fs.rmSync(home, { recursive: true, force: true }));
    fs.writeFileSync(path.join(home, 'secret.txt'), 'TOP-SECRET');
    const rw = path.join(base, 'rw');
    const ro = path.join(base, 'ro');
    fs.mkdirSync(rw);
    fs.mkdirSync(ro);
    fs.writeFileSync(path.join(ro, 'f'), 'x');
    const sandbox = { mode: 'bwrap', network: false, hideHome: true, home: os.homedir(), expose: [], hiddenPaths: [],
      readonlyRoots: ['ro'], limits: { cpu_seconds: 20 } };
    const agent = new Agent(baseConfig({ rw, ro }), { sandbox });
    t.after(() => agent.close());
    const run = (command, root = 'rw') => agent.dispatch('start_process', { command, root, wait_seconds: 5, timeout_seconds: 15 });

    let result = await run(`cat '${home}/secret.txt' 2>&1; echo rc=$?`);
    assert.ok(!result.output.includes('TOP-SECRET'));
    assert.match(result.output, /rc=[1-9]/);

    result = await run('echo ok > new.txt && echo wrote');
    assert.match(result.output, /wrote/);
    assert.equal(fs.readFileSync(path.join(rw, 'new.txt'), 'utf8'), 'ok\n');

    result = await run('echo no > f; echo rc=$?', 'ro');
    assert.match(result.output, /rc=[1-9]/);
    assert.equal(fs.readFileSync(path.join(ro, 'f'), 'utf8'), 'x');

    result = await run('ulimit -t');
    assert.match(result.output, /20/);
    result = await run('wc -l < /proc/net/dev');
    assert.equal(result.output.trim(), '3');

    const job = await agent.dispatch('start_process', { command: 'sleep 61.7', root: 'rw', wait_seconds: 0, timeout_seconds: 30 });
    await agent.dispatch('terminate_process', { job_id: job.job_id });
    await new Promise((resolve) => setTimeout(resolve, 500));
    const leftover = spawnSync('sh', ['-c', "ps -eo args | grep -c '[s]leep 61.7'"], { encoding: 'utf8' });
    assert.equal(leftover.stdout.trim(), '0');
  });


test('local audit requires explicit opt-in and default creates no state', async t => {
  const root = tempDir(t), cfg = baseConfig({ work: root });
  const state = path.join(root, 'state'), file = path.join(root, 'agent.json');
  for (const raw of [null, { version: 1 }, { version: 1, audit: {} }]) {
    if (raw) fs.writeFileSync(hardeningPath(file), JSON.stringify(raw), { mode: 0o600 });
    const options = buildHardening(cfg, file, { env: { XDG_STATE_HOME: state } });
    const agent = new Agent(cfg, options);
    assert.equal(agent.info().hardening.local_audit.enabled, false);
    await agent.dispatch('device_info', {}); await agent.close();
    assert.equal(fs.existsSync(state), false);
  }
});

test('effective limits distinguish implementation capabilities and policy admission', async t => {
  const root = tempDir(t), cfg = { ...baseConfig({ work: root }), allow_shell: false };
  const policy = new Policy({ readonly_roots: ['work'], protected_paths: ['.env'] });
  const agent = new Agent(cfg, { policy, sandbox: { mode: 'none', limits: { cpu_seconds: 5 } } });
  t.after(() => agent.close());
  const info = agent.info(), h = info.hardening;
  assert.ok(info.capabilities.operations.includes('start_process'));
  assert.ok(!h.effective_operations.includes('start_process'));
  assert.deepEqual(h.readonly_roots, ['work']);
  assert.equal(h.sandbox.network, true);
  assert.equal(h.sandbox.limits.cpu_seconds, 5);
  assert.equal(h.policy_revision, agent.info().hardening.policy_revision);
  await assert.rejects(agent.dispatch('write_file', { root: 'work', path: 'a', content: 'secret' }),
    { code: 'policy_root_readonly' });
  agent.policy = new Policy({ protected_paths: ['.env'] });
  assert.notEqual(h.policy_revision, agent.info().hardening.policy_revision);
  await assert.rejects(agent.dispatch('write_file', { root: 'work', path: '.env', content: 'secret' }),
    { code: 'policy_path_protected' });
  await assert.rejects(agent.dispatch('move_file', { root: 'work', source: 'a', path: 'b', destination: 'c' }),
    { code: 'policy_parameters_invalid' });
  agent.policy = new Policy({ allowed_operations: ['read_file'] });
  await assert.rejects(agent.dispatch('write_file', {}), { code: 'policy_operation_denied' });
  agent.policy = new Policy({ protected_paths: Array(64).fill('文'.repeat(250)) });
  const bounded = agent.info().hardening;
  assert.equal(bounded.protected_paths_count, 64);
  assert.equal(bounded.protected_paths_truncated, true);
  assert.ok(Buffer.byteLength(JSON.stringify(bounded)) < 4096);
});
