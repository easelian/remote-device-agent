import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { Files } from '../.build/files.js';
import native from '../.build/posix-fs.js';

test('read/write/edit/list/search preserve protocol fields', async () => {
  const dir = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const files = new Files({ workspace: dir }, true);
  const made = await files.write({ root: 'workspace', path: 'a.txt', content: 'hello 世界\n' });
  assert.equal((await files.read({ root: 'workspace', path: 'a.txt' })).content, 'hello 世界\n');
  await assert.rejects(() => files.write('workspace', 'a.txt', 'overwrite'), /Content conflict/);
  await files.edit({
    root: 'workspace',
    path: 'a.txt',
    old_text: 'hello',
    new_text: 'bye',
    expected_sha256: made.sha256,
  });
  assert.equal((await files.list({ root: 'workspace' })).entries[0].name, 'a.txt');
  assert.equal(
    (await files.search({ root: 'workspace', query: '世界', content: true })).matches[0].line,
    1,
  );
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
  await assert.rejects(
    () => files.read('workspace', 'leaf'),
    /symbolic|symlink|not permitted|operation|not a directory|regular files/i,
  );
  await assert.rejects(
    () => files.read('workspace', 'intermediate/secret'),
    /symbolic|symlink|not permitted|operation|not a directory/i,
  );
  await assert.rejects(
    () => files.list('workspace', 'intermediate'),
    /symbolic|symlink|not permitted|operation|not a directory/i,
  );
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
    await assert.rejects(() => native.listDirectory(fd), /changed|stale|directory/i);
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
  try {
    await files.write('workspace', 'mode.txt', 'after', before.sha256);
  } finally {
    process.umask(previousUmask);
  }
  assert.equal((await fs.stat(target)).mode & 0o777, 0o640);
  await assert.rejects(() => files.read('workspace', '/etc/passwd'), /relative/);
  await assert.rejects(() => files.read('workspace', 'bad\0name'), /relative/);
  await assert.rejects(() => native.openDirectory(files.roots.workspace, '/etc'), /relative/);
  await assert.rejects(() => native.openDirectory(files.roots.workspace, '../outside'), /relative/);
  await assert.rejects(() => native.openDirectory(files.roots.workspace, 'bad\0name'), /relative/);
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
  for (let index = 0; index < 10001; index++) {
    await fs.mkdir(path.join(root, `directory-${index}`));
  }
  const files = new Files({ workspace: root }, false);
  await assert.rejects(() => files.list('workspace'), /exceeds 10000 entries/);
});

test('async directory enumeration yields and honors maxEntries and abort', {
  timeout: 15000,
}, async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  for (let index = 0; index < 1000; index++) {
    await fs.mkdir(path.join(root, `entry-${index}`));
  }
  const handle = await native.openDirectory(await fs.realpath(root));
  t.after(() => native.close(handle));
  let ticked = false;
  const timer = setImmediate(() => {
    ticked = true;
  });
  await assert.rejects(
    () => native.listDirectory(handle, { maxEntries: 10 }),
    /exceeds 10 entries/,
  );
  clearImmediate(timer);
  assert.equal(ticked, true);
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(
    () => native.listDirectory(handle, { signal: controller.signal }),
    /cancel|aborted/i,
  );
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

test('reports file metadata and bounded UTF-8 chunks', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const data = Buffer.from('😀'.repeat(400000));
  await fs.writeFile(path.join(root, 'large.txt'), data);
  const files = new Files({ workspace: root }, false);
  const info = await files.fileInfo({ root: 'workspace', path: 'large.txt' });
  assert.equal(info.type, 'file');
  assert.equal(info.size_bytes, data.length);
  assert.equal(info.read_only, true);
  const chunk = await files.readFileChunk({
    root: 'workspace',
    path: 'large.txt',
    offset: 1,
    length: 17,
  });
  assert.equal(chunk.offset, 1);
  assert.equal(chunk.next_offset, 18);
  assert.equal(chunk.size_bytes, data.length);
  assert.match(chunk.content, /�/);
  assert.equal(typeof chunk.content_base64, 'string');
  assert.equal(chunk.utf8_valid, false);
  assert.equal(chunk.content_encoding, 'utf-8-replacement');
  const complete = await files.readFileChunk({
    root: 'workspace',
    path: 'large.txt',
    offset: 0,
    length: 16,
  });
  assert.equal(complete.utf8_valid, true);
  assert.equal(complete.content_encoding, 'utf-8');
  await assert.rejects(
    () => files.readFileChunk({ root: 'workspace', path: 'large.txt', from_end: 'yes' }),
    /Invalid byte offset|boolean/,
  );
  const fromEnd = await files.readFileChunk({
    root: 'workspace',
    path: 'large.txt',
    offset: 4,
    length: 4,
    from_end: true,
  });
  assert.equal(fromEnd.offset, data.length - 8);
  assert.equal(fromEnd.next_offset, data.length - 4);
  assert.equal((await files.fileInfo({ root: 'workspace', path: '.' })).type, 'directory');
  assert.equal(typeof chunk.fingerprint, 'string');
  await fs.writeFile(path.join(root, 'short.txt'), '0123456789');
  const partialEnd = await files.readFileChunk({
    root: 'workspace',
    path: 'short.txt',
    offset: 8,
    length: 4,
    from_end: true,
  });
  assert.equal(partialEnd.content, '01');
  assert.equal(partialEnd.offset, 0);
  assert.equal(partialEnd.next_offset, 2);
  const beyondEnd = await files.readFileChunk({
    root: 'workspace',
    path: 'short.txt',
    offset: 99,
    length: 4,
    from_end: true,
  });
  assert.equal(beyondEnd.content, '');
  assert.equal(beyondEnd.offset, 0);
  assert.equal(beyondEnd.next_offset, 0);
});

test('multi-read returns local failures and respects the 256 KiB total cap', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.writeFile(path.join(root, 'one.txt'), Buffer.alloc(180000, 0x61));
  await fs.writeFile(path.join(root, 'two.txt'), Buffer.alloc(180000, 0x62));
  await fs.writeFile(path.join(root, 'three.txt'), Buffer.alloc(180000, 0x63));
  await fs.writeFile(path.join(root, 'four.txt'), Buffer.alloc(180000, 0x64));
  await fs.writeFile(path.join(root, 'five.txt'), Buffer.alloc(180000, 0x65));
  const files = new Files({ workspace: root }, false);
  const result = await files.readMultipleFiles({
    files: [
      { root: 'workspace', path: 'one.txt', length: 65536 },
      { root: 'workspace', path: 'missing.txt', length: 65536 },
      { root: 'workspace', path: 'two.txt', length: 65536 },
      { root: 'workspace', path: 'three.txt', length: 65536 },
      { root: 'workspace', path: 'four.txt', length: 65536 },
      { root: 'workspace', path: 'five.txt', length: 65536 },
    ],
    total_limit: 256 * 1024,
  });
  assert.ok(result.total_bytes <= 256 * 1024);
  assert.equal(result.truncated, true);
  assert.equal(result.files[1].path, 'missing.txt');
  assert.match(result.files[1].error, /No such file|ENOENT|regular|not found/i);
});

test('multi-read budgets decoded replacement bytes, not source bytes', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.writeFile(path.join(root, 'invalid.bin'), Buffer.alloc(300000, 0xff));
  const files = new Files({ workspace: root }, false);
  const result = await files.readMultipleFiles({
    files: [{ root: 'workspace', path: 'invalid.bin', length: 65536 }],
    total_limit: 65536,
  });
  assert.ok(result.total_bytes <= 65536);
  assert.equal(result.truncated, false);
  assert.equal(result.total_bytes, 0);
  assert.match(result.files[0].error, /valid UTF-8/);
});

test('multi-read keeps UTF-8 code point and byte budget boundaries', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.writeFile(path.join(root, 'accent.txt'), 'éé');
  await fs.writeFile(path.join(root, 'cjk.txt'), '你好');
  const files = new Files({ workspace: root }, false);
  const accent = await files.readMultipleFiles({
    files: [{ root: 'workspace', path: 'accent.txt', length: 2 }],
    total_limit: 2,
  });
  assert.equal(accent.total_bytes, 2);
  assert.equal(accent.files[0].content, 'é');
  assert.equal(accent.files[0].next_offset, 2);
  const cjkShort = await files.readMultipleFiles({
    files: [{ root: 'workspace', path: 'cjk.txt', length: 6 }],
    total_limit: 3,
  });
  assert.equal(cjkShort.total_bytes, 3);
  assert.equal(cjkShort.files[0].content, '你');
  const cjkFull = await files.readMultipleFiles({
    files: [{ root: 'workspace', path: 'cjk.txt', length: 6 }],
    total_limit: 6,
  });
  assert.equal(cjkFull.total_bytes, 6);
  assert.equal(cjkFull.files[0].content, '你好');
  await fs.writeFile(path.join(root, 'emoji.txt'), '😀');
  const partialEmoji = await files.readMultipleFiles({
    files: [{ root: 'workspace', path: 'emoji.txt', length: 1 }],
    total_limit: 16,
  });
  assert.match(partialEmoji.files[0].error, /complete UTF-8|invalid UTF-8/);
  await fs.writeFile(path.join(root, 'too-large.bin'), Buffer.alloc(2 * 1024 * 1024));
  const tooLarge = await files.readMultipleFiles({
    files: [{ root: 'workspace', path: 'too-large.bin', length: 1 }],
    total_limit: 16,
  });
  assert.match(tooLarge.files[0].error, /exceeds 1 MiB/);
});

test('moves only regular files within one root and refuses conflicts', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  const other = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-other-'));
  t.after(async () => {
    await fs.rm(root, { recursive: true, force: true });
    await fs.rm(other, { recursive: true, force: true });
  });
  await fs.writeFile(path.join(root, 'source.txt'), 'move me');
  await fs.writeFile(path.join(root, 'existing.txt'), 'keep');
  const files = new Files({ workspace: root, other }, true);
  const moved = await files.moveFile({
    root: 'workspace',
    source: 'source.txt',
    destination: 'moved.txt',
  });
  assert.equal(moved.moved, true);
  assert.equal(await fs.readFile(path.join(root, 'moved.txt'), 'utf8'), 'move me');
  await assert.rejects(
    () => files.moveFile({ root: 'workspace', source: 'moved.txt', destination: 'existing.txt' }),
    /exists|Destination/,
  );
  assert.equal(await fs.readFile(path.join(root, 'existing.txt'), 'utf8'), 'keep');
  await assert.rejects(
    () =>
      files.moveFile({ root: 'workspace', source: 'moved.txt', destination: 'other/moved.txt' }),
    /Invalid|directory|not found/,
  );
  await assert.rejects(
    () =>
      files.moveFile({
        root: 'workspace',
        path: 'moved.txt',
        destination_root: 'other',
        destination_path: 'moved.txt',
      }),
    /across roots/,
  );
});

test('serializes concurrent moves of the same source', async (t) => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-files-'));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.writeFile(path.join(root, 'source.txt'), 'one winner');
  const files = new Files({ workspace: root }, true);
  const outcomes = await Promise.allSettled([
    files.moveFile({
      root: 'workspace',
      path: 'source.txt',
      destination_root: 'workspace',
      destination_path: 'a.txt',
    }),
    files.moveFile({
      root: 'workspace',
      path: 'source.txt',
      destination_root: 'workspace',
      destination_path: 'b.txt',
    }),
  ]);
  assert.equal(outcomes.filter((outcome) => outcome.status === 'fulfilled').length, 1);
  assert.equal(outcomes.filter((outcome) => outcome.status === 'rejected').length, 1);
});
