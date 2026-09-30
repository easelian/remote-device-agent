import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { Files } from '../lib/files.mjs';
import native from '../lib/posix-fs.mjs';

test('read/write/edit/list/search preserve protocol fields', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const files = new Files({ workspace: dir }, true);
  const made = await files.write({ root: 'workspace', path: 'a.txt', content: 'hello 世界\n' });
  assert.equal((await files.read({ root: 'workspace', path: 'a.txt' })).content, 'hello 世界\n');
  await assert.rejects(() => files.write('workspace', 'a.txt', 'overwrite'), /Content conflict/);
  await files.edit({ root: 'workspace', path: 'a.txt', old_text: 'hello', new_text: 'bye', expected_sha256: made.sha256 });
  assert.equal((await files.list({ root: 'workspace' })).entries[0].name, 'a.txt');
  assert.equal((await files.search({ root: 'workspace', query: '世界', content: true })).matches[0].line, 1);
});

test('rejects traversal and honors read-only mode', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const files = new Files({ workspace: dir }, false);
  await assert.rejects(() => files.read('workspace', '../secret'), /relative/);
  await assert.rejects(() => files.mkdir('workspace', 'new'), /disabled/);
  assert.equal(await files.checkedDirectory('workspace', '.'), await fs.realpath(dir));
});

test('rejects symlink leaves and intermediate components', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const outside = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-outside-'));
  await fs.writeFile(path.join(outside, 'secret'), 'outside');
  await fs.mkdir(path.join(dir, 'real'));
  await fs.symlink(path.join(outside, 'secret'), path.join(dir, 'leaf'));
  await fs.symlink(outside, path.join(dir, 'intermediate'));
  const files = new Files({ workspace: dir }, true);
  await assert.rejects(() => files.read('workspace', 'leaf'), /symbolic|symlink|not permitted|operation|not a directory|regular files/i);
  await assert.rejects(() => files.read('workspace', 'intermediate/secret'), /symbolic|symlink|not permitted|operation|not a directory/i);
  await assert.rejects(() => files.list('workspace', 'intermediate'), /symbolic|symlink|not permitted|operation|not a directory/i);
});

test('rejects a directory handle after its path is exchanged', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const oldDir = path.join(dir, 'work');
  await fs.mkdir(oldDir);
  await fs.writeFile(path.join(oldDir, 'old.txt'), 'old');
  const fd = await native.openDirectory(await fs.realpath(dir), 'work');
  try {
    await fs.rename(oldDir, path.join(dir, 'moved'));
    await fs.mkdir(oldDir);
    await fs.writeFile(path.join(oldDir, 'new.txt'), 'new');
    assert.throws(() => native.listDirectory(fd), /changed|stale|directory/i);
  } finally {
    await native.close(fd);
  }
});

test('preserves existing file permissions and rejects invalid paths', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const target = path.join(dir, 'mode.txt');
  await fs.writeFile(target, 'before', { mode: 0o640 });
  await fs.chmod(target, 0o640);
  const files = new Files({ workspace: dir }, true);
  const before = await files.read('workspace', 'mode.txt');
  const previousUmask = process.umask(0o077);
  try { await files.write('workspace', 'mode.txt', 'after', before.sha256); }
  finally { process.umask(previousUmask); }
  assert.equal((await fs.stat(target)).mode & 0o777, 0o640);
  await assert.rejects(() => files.read('workspace', '/etc/passwd'), /relative/);
  await assert.rejects(() => files.read('workspace', 'bad\0name'), /relative/);
  assert.throws(() => native.openDirectory(files.roots.workspace, '/etc'), /relative/);
  assert.throws(() => native.openDirectory(files.roots.workspace, '../outside'), /relative/);
  assert.throws(() => native.openDirectory(files.roots.workspace, 'bad\0name'), /relative/);
});

test('rejects a configured root after replacing its canonical path with a symlink', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const outside = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-outside-'));
  const files = new Files({ workspace: root }, false);
  await fs.rename(root, `${root}-moved`);
  await fs.symlink(outside, root);
  await assert.rejects(() => files.list('workspace'), /canonical|changed|root/i);
});

test('bounds directory enumeration at 10000 entries', { timeout: 15000 }, async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  for (let index = 0; index < 10001; index++) await fs.mkdir(path.join(root, `directory-${index}`));
  const files = new Files({ workspace: root }, false);
  await assert.rejects(() => files.list('workspace'), /exceeds 10000 entries/);
});

test('matches Python casefold semantics and sorts by Unicode code point', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  for (const name of ['z', 'ä', 'a', '\u{10000}', '\uE000', 'Straße', 'ς', 'Ꭰ']) {
    await fs.writeFile(path.join(dir, name), 'x');
  }
  const files = new Files({ workspace: dir }, false);
  const entries = (await files.list('workspace')).entries.map((entry) => entry.name);
  assert.deepEqual(entries, ['Straße', 'a', 'z', 'ä', 'ς', 'Ꭰ', '\uE000', '\u{10000}']);
  assert.equal((await files.search('workspace', 'STRASSE')).matches[0].path, 'Straße');
  assert.equal((await files.search('workspace', 'Σ')).matches[0].path, 'ς');
  assert.equal((await files.search('workspace', 'ꭰ')).matches[0].path, 'Ꭰ');
});
