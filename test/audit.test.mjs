import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { AuditLog } from '../.build/audit.js';
import { Agent } from '../.build/agent.js';
import { AgentConfig } from '../.build/config.js';
import { buildHardening, hardeningPath } from '../.build/hardening.js';
import { assertPrivatePath, ensurePrivatePath } from '../.build/platform.js';

function fixture(t, options = {}) {
  const directory = fs.mkdtempSync(path.join(fs.realpathSync(os.tmpdir()), 'rdh-audit-'));
  ensurePrivatePath(directory, true);
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const open = () => {
    const log = new AuditLog({ directory, name: 'device', ...options });
    t.after(() => log.close());
    return log;
  };
  return { directory, open };
}

const record = (log, id) =>
  log.record({
    id: String(id),
    operation: 'read_file',
    status: 'ok',
    params: { path: '世界.txt' },
  });
const entries = (file) =>
  fs.readFileSync(file, 'utf8').trim().split('\n').filter(Boolean).map(JSON.parse);
const digest = (line) => createHash('sha256').update(`${line}\n`).digest('hex').slice(0, 16);

test('audit retains exactly the latest 1000 JSON lines across writes and restarts', (t) => {
  const { open } = fixture(t);
  let log = open();
  for (let id = 0; id < 1003; id++) {
    record(log, id);
  }
  const first = entries(log.file);
  assert.deepEqual(
    first.map((entry) => entry.id),
    Array.from({ length: 1000 }, (_, i) => String(i + 3)),
  );
  const raw = fs.readFileSync(log.file, 'utf8').trimEnd().split('\n');
  for (let index = 1; index < raw.length; index++) {
    assert.equal(JSON.parse(raw[index]).prev, digest(raw[index - 1]));
  }
  const oldBoot = first.at(-1).boot;
  log.close();
  log = open();
  record(log, 1003);
  const restarted = entries(log.file);
  assert.equal(restarted.length, 1000);
  assert.equal(restarted[0].id, '4');
  assert.equal(restarted.at(-1).id, '1003');
  assert.notEqual(restarted.at(-1).boot, oldBoot);
  assert.equal(log.stats().dropped, 0);
  assertPrivatePath(log.file, false);
});

test('the default Agent records successful and failed requests locally without a sidecar', async (t) => {
  const { directory } = fixture(t);
  const config = AgentConfig.validate({
    gateway_url: 'http://127.0.0.1:9/',
    device_id: 'default-audit-device',
    token: 't'.repeat(48),
    roots: { work: directory },
    allow_shell: false,
    allow_write: false,
  });
  const env = { XDG_STATE_HOME: directory, LOCALAPPDATA: directory };
  const file = path.join(directory, 'agent.json');
  const options = buildHardening(config, file, { env });
  const agent = new Agent(config, options);
  t.after(() => agent.close());
  assert.equal(agent.info().hardening.local_audit.enabled, true);
  await agent.dispatch('device_info', {}, { requestId: 'successful' });
  await assert.rejects(
    agent.dispatch('read_file', { root: 'work', path: 'missing' }, { requestId: 'failed' }),
  );
  await agent.close();
  assert.deepEqual(
    entries(options.audit.file).map(({ id, status }) => ({ id, status })),
    [
      { id: 'successful', status: 'ok' },
      { id: 'failed', status: 'error' },
    ],
  );
  const policyFile = hardeningPath(file);
  ensurePrivatePath(policyFile, false);
  fs.writeFileSync(policyFile, JSON.stringify({ version: 1, audit: { enabled: false } }));
  assert.equal(buildHardening(config, file, { env }).audit, null);
});

test('audit applies the 1000-record cap to current and rotated files together', (t) => {
  const { directory, open } = fixture(t, { maxBytes: 160 * 1024, maxFiles: 3 });
  const log = open();
  for (let id = 0; id < 1003; id++) {
    record(log, id);
  }
  const files = fs.readdirSync(directory).filter((name) => /^device\.jsonl(?:\.\d+)?$/.test(name));
  assert.ok(files.length > 1);
  const all = files.flatMap((name) => entries(path.join(directory, name)));
  assert.equal(all.length, 1000);
  assert.deepEqual(
    all.map((entry) => Number(entry.id)).sort((a, b) => a - b),
    Array.from({ length: 1000 }, (_, i) => i + 3),
  );
  for (const file of files) {
    assert.ok(fs.statSync(path.join(directory, file)).size <= log.maxBytes);
  }
  assert.equal(log.stats().dropped, 0);
});

test('startup prunes legacy archives and incomplete tails without touching other devices', (t) => {
  const { directory, open } = fixture(t);
  const file = path.join(directory, 'device.jsonl');
  const lines = Array.from({ length: 1100 }, (_, id) => JSON.stringify({ id: String(id) }));
  const write = (name, content) => {
    const target = path.join(directory, name);
    ensurePrivatePath(target, false);
    fs.writeFileSync(target, content);
  };
  write('device.jsonl', `${lines.slice(600).join('\n')}\n{"incomplete":`);
  write('device.jsonl.1', `${lines.slice(300, 600).join('\n')}\n`);
  write('device.jsonl.4', `${lines.slice(0, 300).join('\n')}\n`);
  write('device.jsonl.5', '{"id":"older"}\n');
  write('other-device.jsonl', '{"keep":true}\n');
  const log = open();
  assert.equal(log.retained, 1000);
  assert.equal(entries(path.join(directory, 'device.jsonl.4'))[0].id, '100');
  assert.equal(fs.existsSync(path.join(directory, 'device.jsonl.5')), false);
  assert.equal(
    fs.readFileSync(path.join(directory, 'other-device.jsonl'), 'utf8'),
    '{"keep":true}\n',
  );
  record(log, 'next');
  assert.equal(entries(file).at(-1).id, 'next');
  assert.equal(log.stats().dropped, 0);
});

test('failed compaction preserves history and permits the next record to recover', (t) => {
  const { directory, open } = fixture(t);
  const log = open();
  for (let id = 0; id < 1000; id++) {
    record(log, id);
  }
  const before = fs.readFileSync(log.file, 'utf8');
  const rename = fs.renameSync;
  const mock = t.mock.method(fs, 'renameSync', (source, destination) => {
    if (destination === log.file) {
      throw Object.assign(new Error('injected'), { code: 'ENOSPC' });
    }
    return rename(source, destination);
  });
  record(log, 'failed');
  assert.equal(log.stats().dropped, 1);
  assert.equal(fs.readFileSync(log.file, 'utf8'), before);
  assert.deepEqual(fs.readdirSync(directory), ['device.jsonl']);
  mock.mock.restore();
  record(log, 'recovered');
  assert.equal(entries(log.file).length, 1000);
  assert.equal(entries(log.file)[0].id, '1');
  assert.equal(entries(log.file).at(-1).id, 'recovered');
  assert.equal(log.stats().written, 1001);
});
