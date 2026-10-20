import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { createHash } from 'node:crypto';
import { Agent } from '../.build/agent.mjs';
import { AuditLog } from '../.build/audit.mjs';
import { buildHardening, hardeningPath } from '../.build/hardening.mjs';
import { Policy } from '../.build/policy.mjs';
import { defaultHiddenPaths, probeBwrap } from '../.build/sandbox.mjs';

const mkdir = p => { fs.mkdirSync(p, { recursive: true, mode: 0o700 }); return p; };
function temporary(t, parent = os.tmpdir()) {
  const directory = fs.mkdtempSync(path.join(parent, 'rdh-hardening-regression-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  return directory;
}
const config = roots => ({ gateway_url: 'http://127.0.0.1:9/', device_id: 'regression-device', token: 't'.repeat(48),
  roots, allow_shell: true, allow_write: true, shell: '/bin/sh', max_processes: 4, max_process_seconds: 30 });
const quote = s => `'${s.replaceAll("'", "'\\''")}'`;
const sandbox = (home, extra = {}) => ({ mode: 'bwrap', network: false, hideHome: true, home,
  expose: [], hiddenPaths: [], hiddenFiles: [], readonlyRoots: [], limits: {}, ...extra });
const run = (agent, command, root = 'work') => agent.dispatch('start_process', { command, root, wait_seconds: 1, timeout_seconds: 10 });
function agentFor(t, roots, options) {
  const agent = new Agent(config(roots), options);
  t.after(() => agent.close());
  return agent;
}
const entry = (params = {}) => ({ operation: 'read_file', status: 'ok', params });
function journal(t, maxBytes = 4096) {
  const log = new AuditLog({ directory: path.join(temporary(t), 'audit'), maxBytes });
  t.after(() => log.close());
  return log;
}
const lines = log => fs.readFileSync(log.file, 'utf8').trim().split('\n').map(JSON.parse);
const hasBwrap = (() => { try { probeBwrap(); return true; } catch { return false; } })();
const bwrapTest = { skip: !hasBwrap && 'bwrap unavailable' };

test('move policy rejects mixed aliases and malformed pairs without touching protected files', async t => {
  const root = temporary(t);
  fs.writeFileSync(path.join(root, '.env'), 'protected');
  const agent = agentFor(t, { work: root }, { policy: new Policy({ protected_paths: ['.env'] }) });
  for (const value of [123, false, null, {}, [], 'safe']) {
    await assert.rejects(agent.dispatch('move_file', { root: 'work', source: value, path: '.env', destination_root: 'work', destination_path: 'out' }));
    await assert.rejects(agent.dispatch('move_file', { root: 'work', path: 'safe', destination_root: 'work', destination: value, destination_path: '.env' }));
  }
  for (const params of [{ source: '.env', destination: 'out' }, { path: '.env', destination_root: 'work', destination_path: 'out' },
    { source: 'safe', destination: '.env' }, { path: 'safe', destination_root: 'work', destination_path: '.env' }]) {
    await assert.rejects(agent.dispatch('move_file', { root: 'work', ...params }), /protected/);
  }
  assert.equal(fs.readFileSync(path.join(root, '.env'), 'utf8'), 'protected');
  assert.equal(fs.existsSync(path.join(root, 'out')), false);
  fs.writeFileSync(path.join(root, 'safe'), 'ordinary');
  await agent.dispatch('move_file', { root: 'work', source: 'safe', destination: 'modern' });
  await agent.dispatch('move_file', { root: 'work', path: 'modern', destination_root: 'work', destination_path: 'legacy' });
  await agent.files.moveFile('work', 'legacy', 'work', 'positional');
  assert.equal(fs.readFileSync(path.join(root, 'positional'), 'utf8'), 'ordinary');
  await assert.rejects(agent.files.moveFile({ root: 'work', source: 123, path: '.env', destination_root: 'work', destination_path: 'out' }));
});

test('sandbox masks survive HOME roots and exposure symlink aliases', bwrapTest, async t => {
  const home = temporary(t);
  const cfg = mkdir(path.join(home, '.config/remote-device-hub'));
  const audit = mkdir(path.join(home, '.local/state/remote-device-hub/audit'));
  const token = path.join(cfg, 'device.json'), trail = path.join(audit, 'device.jsonl');
  fs.writeFileSync(token, 'FAKE-TOKEN'); fs.writeFileSync(trail, 'ORIGINAL-AUDIT');
  const alias = path.join(home, 'config-alias'); fs.symlinkSync(cfg, alias);
  const agent = agentFor(t, { work: home }, { sandbox: sandbox(home, { expose: [alias], hiddenPaths: defaultHiddenPaths({}, home) }) });
  const result = await run(agent, `cat ${quote(token)} ${quote(path.join(alias, 'device.json'))}; printf tampered > ${quote(trail)}`);
  assert.ok(!result.output.includes('FAKE-TOKEN'));
  assert.equal(fs.readFileSync(trail, 'utf8'), 'ORIGINAL-AUDIT');
  const normal = await run(agent, 'printf ordinary > allowed.txt');
  assert.equal(normal.exit_code, 0);
  assert.equal(fs.readFileSync(path.join(home, 'allowed.txt'), 'utf8'), 'ordinary');
});

test('actual hardening hides custom configuration and newly created audit state', bwrapTest, async t => {
  // Outside /tmp and HOME: the root filesystem alone used to expose this file.
  const base = temporary(t, '/var/tmp');
  const privateDir = mkdir(path.join(base, 'private'));
  const alias = path.join(base, 'private-alias'); fs.symlinkSync(privateDir, alias);
  const configFile = path.join(alias, 'device.json');
  const work = mkdir(path.join(base, 'work'));
  const cfg = config({ work, all: base });
  fs.writeFileSync(configFile, JSON.stringify({ ...cfg, token: 'FAKE-CUSTOM-TOKEN' }), { mode: 0o600 });
  fs.writeFileSync(hardeningPath(configFile), JSON.stringify({ version: 1, audit: { enabled: true }, sandbox: { mode: 'bwrap', network: false } }), { mode: 0o600 });
  const env = { XDG_CONFIG_HOME: path.join(base, 'config'), XDG_STATE_HOME: path.join(base, 'new-state') };
  const options = buildHardening(cfg, configFile, { env });
  const agent = new Agent(cfg, options); t.after(() => agent.close());
  options.audit.record(entry());
  const trail = options.audit.file;
  const result = await run(agent, `cat ${quote(configFile)} ${quote(path.join(privateDir, 'device.json'))} ${quote(hardeningPath(configFile))}; printf tampered > ${quote(trail)}`);
  assert.ok(!result.output.includes('FAKE-CUSTOM-TOKEN'));
  assert.ok(!result.output.includes('"version"'));
  assert.equal(JSON.parse(fs.readFileSync(configFile, 'utf8')).token, 'FAKE-CUSTOM-TOKEN');
  assert.ok(!fs.readFileSync(trail, 'utf8').includes('tampered'));
  assert.ok(lines(options.audit).length >= 2);
  assert.equal((await run(agent, 'printf allowed > ordinary')).exit_code, 0);
});

test('readonly mounts win for both root orders, aliases and writable descendants', bwrapTest, async t => {
  const base = temporary(t), docs = mkdir(path.join(base, 'docs')), child = mkdir(path.join(docs, 'child'));
  const variants = [
    { roots: { docs, work: base }, readonlyRoots: ['docs'] },
    { roots: { work: base, docs }, readonlyRoots: ['docs'] },
    { roots: { docs, work: base, alias: docs }, readonlyRoots: ['docs'] },
    { roots: { work: base, child, docs }, readonlyRoots: ['docs'] },
  ];
  for (const variant of variants) {
    const agent = agentFor(t, variant.roots, { sandbox: sandbox(path.join(base, 'home'), variant) });
    const target = path.join(child, 'forbidden');
    assert.notEqual((await run(agent, `printf changed > ${quote(target)}`)).exit_code, 0);
    assert.equal(fs.existsSync(target), false);
    assert.equal((await run(agent, 'printf allowed > ordinary')).exit_code, 0);
    await agent.close();
  }
});

test('audit recovers after transient rotation open failure without reopening after close', t => {
  const log = journal(t);
  log.record(entry());
  fs.appendFileSync(log.file, ' '.repeat(log.maxBytes - log.size)); log.size = log.maxBytes;
  const original = fs.openSync;
  const mock = t.mock.method(fs, 'openSync', function(file, ...args) {
    if (file === log.file) throw Object.assign(new Error('injected'), { code: 'ENOSPC' });
    return original.call(this, file, ...args);
  });
  log.record(entry());
  assert.equal(log.stats().dropped, 1); assert.equal(log.fd, -1);
  mock.mock.restore();
  log.record(entry({ path: 'after-recovery' }));
  assert.equal(log.stats().written, 2); assert.equal(log.stats().dropped, 1);
  assert.equal(lines(log)[0].params.path, 'after-recovery');
  log.close(); log.record(entry());
  assert.equal(log.fd, -1); assert.equal(log.stats().written, 2);
});

test('audit completes short writes with byte offsets and preserves the hash chain', t => {
  const log = journal(t), original = fs.writeSync;
  t.mock.method(fs, 'writeSync', function(fd, data, offset, length) {
    return fd === log.fd ? original.call(this, fd, data, offset, Math.min(7, length)) : original.call(this, fd, data, offset, length);
  });
  log.record(entry({ path: '世界/é' })); log.record(entry({ path: 'next' }));
  const raw = fs.readFileSync(log.file, 'utf8').trimEnd().split('\n');
  assert.equal(JSON.parse(raw[0]).params.path, '世界/é');
  assert.equal(JSON.parse(raw[1]).prev, createHash('sha256').update(`${raw[0]}\n`).digest('hex').slice(0, 16));
  assert.equal(log.size, fs.statSync(log.file).size);
  assert.equal(log.stats().written, 2); assert.equal(log.stats().dropped, 0);
});

test('audit repairs a partial record before appending after write and repair failures', t => {
  const log = journal(t); log.record(entry({ path: 'before' }));
  const original = fs.writeSync; let calls = 0;
  const writeMock = t.mock.method(fs, 'writeSync', function(fd, data, offset, length) {
    if (fd !== log.fd) return original.call(this, fd, data, offset, length);
    if (calls++ === 0) return original.call(this, fd, data, offset, 20);
    throw Object.assign(new Error('injected'), { code: 'ENOSPC' });
  });
  const truncateMock = t.mock.method(fs, 'ftruncateSync', () => { throw new Error('injected repair failure'); });
  log.record(entry({ path: 'incomplete' }));
  assert.equal(log.stats().written, 1); assert.equal(log.stats().dropped, 1);
  writeMock.mock.restore();
  log.record(entry({ path: 'still-cannot-repair' }));
  assert.equal(log.stats().written, 1); assert.equal(log.stats().dropped, 2);
  truncateMock.mock.restore();
  log.record(entry({ path: 'recovered' }));
  assert.deepEqual(lines(log).map(line => line.params.path), ['before', 'recovered']);
  assert.equal(log.stats().written, 2);
});

test('zero-progress audit writes fail promptly and leave no incomplete line', t => {
  const log = journal(t);
  const mock = t.mock.method(fs, 'writeSync', () => 0);
  log.record(entry());
  assert.equal(log.stats().dropped, 1); assert.equal(log.stats().written, 0);
  assert.equal(fs.statSync(log.file).size, 0);
  mock.mock.restore(); log.record(entry());
  assert.equal(lines(log).length, 1);
});

test('every audit file and complete record fits the byte limit including Unicode metadata', t => {
  const log = journal(t);
  for (const char of ['a', '界']) {
    log.record({ ...entry({ paths: Array(16).fill(char.repeat(256)) }), id: char.repeat(128), error: char.repeat(300),
      result: Object.fromEntries(['sha256', 'status', 'job_id', 'search_id', 'size_bytes', 'total', 'pid'].map(key => [key, char.repeat(128)])) });
  }
  for (const file of fs.readdirSync(log.directory)) {
    const raw = fs.readFileSync(path.join(log.directory, file), 'utf8');
    assert.ok(Buffer.byteLength(raw) <= 4096);
    for (const line of raw.trim().split('\n').filter(Boolean)) assert.ok(JSON.parse(line));
  }
  assert.equal(log.stats().written, 2); assert.equal(log.stats().dropped, 0);
});

test('enabled audit never records inline command or query secrets on success error or denial', async t => {
  const root = temporary(t), cfg = config({ work: root });
  fs.writeFileSync(hardeningPath(path.join(root, 'absent.json')), JSON.stringify({ version: 1, audit: { enabled: true } }), { mode: 0o600 });
  const options = buildHardening(cfg, path.join(root, 'absent.json'), { env: { XDG_STATE_HOME: path.join(root, 'state') } });
  const agent = new Agent(cfg, options); t.after(() => agent.close());
  const command = 'printf %s FAKE-INLINE-SECRET';
  await run(agent, command);
  await assert.rejects(run(agent, command, 'missing'));
  agent.policy = new Policy({ allowed_operations: ['device_info'] });
  await assert.rejects(run(agent, command), /not allowed/);
  await assert.rejects(agent.dispatch('search_files', { root: 'work', query: 'FAKE-QUERY-SECRET' }));
  const raw = fs.readFileSync(options.audit.file, 'utf8');
  assert.ok(!raw.includes('FAKE-INLINE-SECRET')); assert.ok(!raw.includes('FAKE-QUERY-SECRET'));
  const records = lines(options.audit);
  assert.deepEqual(records.slice(0, 3).map(record => record.status), ['ok', 'error', 'denied']);
  assert.equal(records[0].params.command_sha256, createHash('sha256').update(command).digest('hex'));
  assert.equal(records[0].params.command_bytes, Buffer.byteLength(command));
});

test('ancestor roots and proc exposures cannot restore host procfs or private HOME and tmp', bwrapTest, async t => {
  const base = temporary(t, '/var/tmp'), work = mkdir(path.join(base, 'work'));
  const fakeHome = mkdir(path.join(base, 'home'));
  const secret = path.join(fakeHome, 'device.json'); fs.writeFileSync(secret, 'FAKE-PROC-TOKEN');
  const hostTemp = temporary(t); const tempSecret = path.join(hostTemp, 'secret'); fs.writeFileSync(tempSecret, 'FAKE-TMP-SECRET');
  for (const extra of [
    { roots: { work, proc: '/proc' }, readonlyRoots: ['proc'] },
    { roots: { work, all: '/' }, readonlyRoots: ['all'] },
    { roots: { work }, expose: ['/proc'] },
    { roots: { work }, expose: ['/'] },
  ]) {
    const agent = agentFor(t, extra.roots, { sandbox: sandbox(fakeHome, { ...extra, hiddenFiles: [secret] }) });
    const result = await run(agent, `cat ${quote(`/proc/${process.pid}/root${secret}`)} ${quote(secret)} ${quote(tempSecret)}; printf SANDBOX-RAN`);
    assert.match(result.output, /SANDBOX-RAN/);
    assert.ok(!result.output.includes('FAKE-PROC-TOKEN'), `host proc restored: ${JSON.stringify(extra)}`);
    assert.ok(!result.output.includes('FAKE-TMP-SECRET'), `host tmp restored: ${JSON.stringify(extra)}`);
    await agent.close();
  }
});
