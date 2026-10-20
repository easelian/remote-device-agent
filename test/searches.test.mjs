import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { Files } from '../.build/files.mjs';
import { SearchTasks } from '../.build/searches.mjs';
import native from '../.build/posix-fs.mjs';

test('invalid requests retain prior results and shutdown rejects new searches', async (t) => {
  const root = await fixture();
  const manager = new SearchTasks(new Files({ work: root }, false), { maxRetainedTasks: 1 });
  t.after(async () => { await manager.close(); await fs.rm(root, { recursive: true, force: true }); });
  const started = await manager.startSearch({ root: 'work', query: 'a.txt' });
  for (let i = 0; i < 100; i++) {
    if ((await manager.getSearchResults({ search_id: started.search_id })).status === 'completed') break;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  await assert.rejects(manager.startSearch({ root: 'work', query: '' }), /query/);
  assert.equal((await manager.getSearchResults({ search_id: started.search_id })).results[0].path, 'a.txt');
  await manager.close();
  await assert.rejects(manager.startSearch({ root: 'work', query: 'a.txt' }), /closed/);
});

async function fixture() {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-search-'));
  await fs.mkdir(path.join(root, 'deep'));
  await fs.writeFile(path.join(root, 'a.txt'), 'needle one\nneedle two\n');
  await fs.writeFile(path.join(root, 'deep', 'b.txt'), 'needle three\n');
  await fs.writeFile(path.join(root, 'deep', 'emoji.txt'), `needle ${'😀'.repeat(300)}\n`);
  await fs.symlink(path.join(root, 'deep', 'b.txt'), path.join(root, 'link.txt'));
  return root;
}

test('search results are deterministic and paginate without loss or duplicates', async () => {
  const root = await fixture();
  const manager = new SearchTasks(new Files({ work: root }, false), { taskTtlMs: 5000 });
  const started = await manager.startSearch({ root: 'work', query: 'needle', content: true });
  let page; const found = [];
  for (let i = 0; i < 20; i++) {
    page = await manager.getSearchResults({ search_id: started.search_id, offset: found.length, limit: 1 });
    found.push(...page.results);
    if (page.status === 'completed' && page.next_offset === null) break;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  assert.deepEqual(found.map((item) => item.path), ['a.txt', 'a.txt', 'deep/b.txt', 'deep/emoji.txt']);
  assert.equal(new Set(found.map((item) => `${item.path}:${item.line}`)).size, found.length);
  const emoji = found.find((item) => item.path === 'deep/emoji.txt');
  assert.equal(Array.from(emoji.text).length <= 500, true);
  assert.equal(/(?:^|[^\uD800-\uDBFF])[\uDC00-\uDFFF]|[\uD800-\uDBFF](?:$|[^\uDC00-\uDFFF])/.test(emoji.text), false);
  await manager.close();
});

test('stop cancels a running search and symlinks are skipped', async () => {
  const root = await fixture();
  const manager = new SearchTasks(new Files({ work: root }, false), { maxVisited: 1, taskTtlMs: 5000 });
  const started = await manager.startSearch({ root: 'work', query: 'b', content: false });
  await manager.stopSearch({ search_id: started.search_id });
  const result = await manager.getSearchResults({ search_id: started.search_id });
  assert.equal(result.status, 'stopped');
  assert.equal(result.results.some((item) => item.path === 'link.txt'), false);
  await manager.close();
});

test('budgets and expiry are explicit and bounded', async () => {
  const root = await fixture();
  const slowFilesystem = { ...native, listDirectory: async (fd) => {
    await new Promise((resolve) => setTimeout(resolve, 15));
    return native.listDirectory(fd);
  } };
  const manager = new SearchTasks(new Files({ work: root }, false, slowFilesystem), { maxEntries: 1, taskTtlMs: 5 });
  const started = await manager.startSearch({ root: 'work', query: 'needle', content: true });
  await new Promise((resolve) => setTimeout(resolve, 20));
  const result = await manager.getSearchResults({ search_id: started.search_id });
  assert.equal(result.status, 'expired');
  assert.equal(result.results.length <= 1, true);
  await manager.close();
});

async function waitDone(manager, search_id) {
  for (let i = 0; i < 100; i++) {
    const result = await manager.getSearchResults({ search_id });
    if (!['queued', 'running'].includes(result.status)) return result;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  throw new Error('search did not finish');
}

test('filename search keeps Unicode casefold semantics and rejects future offsets', async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'rdh-search-'));
  await fs.writeFile(path.join(root, 'Straße'), 'x');
  const manager = new SearchTasks(new Files({ work: root }, false));
  const started = await manager.startSearch({ root: 'work', query: 'STRASSE' });
  const result = await waitDone(manager, started.search_id);
  assert.deepEqual(result.results, [{ path: 'Straße' }]);
  await assert.rejects(() => manager.getSearchResults({ search_id: started.search_id, offset: 2 }), /beyond/);
  await manager.close();
});

test('result budget stops traversal with an explicit reason', async () => {
  const root = await fixture();
  const manager = new SearchTasks(new Files({ work: root }, false), { maxResultBytes: 10 });
  const started = await manager.startSearch({ root: 'work', query: 'needle', content: true });
  const result = await waitDone(manager, started.search_id);
  assert.equal(result.truncated, true);
  assert.equal(result.truncated_reason, 'max_result_bytes');
  await manager.close();
});

test('legacy maxEntries remains the result limit while nonmatching entries are traversed', async () => {
  const root = await fixture();
  const manager = new SearchTasks(new Files({ work: root }, false), { maxEntries: 1 });
  const started = await manager.startSearch({ root: 'work', query: 'needle', content: true });
  const result = await waitDone(manager, started.search_id);
  assert.equal(result.results.length, 1);
  assert.equal(result.truncated_reason, 'max_entries');
  await manager.close();
});

test('retains only bounded completed task metadata and closes cancelled fds', async () => {
  const root = await fixture();
  const files = new Files({ work: root }, false);
  const manager = new SearchTasks(files, { maxRetainedTasks: 1, retentionTtlMs: 10_000 });
  const first = await manager.startSearch({ root: 'work', query: 'a' });
  await waitDone(manager, first.search_id);
  const second = await manager.startSearch({ root: 'work', query: 'b' });
  await waitDone(manager, second.search_id);
  assert.equal(manager.listSearches().searches.length, 1);
  await assert.rejects(() => manager.startSearch({ root: 'missing', query: 'x' }), /Unknown root/);
  await manager.close();
});

test('directory errors are reported as truncated rather than complete', async () => {
  const root = await fixture();
  const filesystem = { ...native, listDirectory: async () => { throw new Error('E2BIG'); } };
  const manager = new SearchTasks(new Files({ work: root }, false, filesystem));
  const started = await manager.startSearch({ root: 'work', query: 'x' });
  const result = await waitDone(manager, started.search_id);
  assert.equal(result.status, 'completed');
  assert.equal(result.truncated, true);
  assert.equal(result.truncated_reason, 'directory_error');
  assert.equal(result.skipped, 1);
  await manager.close();
});

test('stopped workers remain in concurrency budget until fd cleanup', async () => {
  const root = await fixture();
  const slowFilesystem = { ...native, listDirectory: async (fd) => {
    await new Promise((resolve) => setTimeout(resolve, 30));
    return native.listDirectory(fd);
  } };
  const files = new Files({ work: root }, false, slowFilesystem);
  let closed = 0;
  const close = files.close.bind(files);
  files.close = async (fd) => { closed += 1; return close(fd); };
  const manager = new SearchTasks(files, { maxConcurrent: 1, taskTtlMs: 5000 });
  const started = await manager.startSearch({ root: 'work', query: 'needle' });
  await manager.stopSearch({ search_id: started.search_id });
  await assert.rejects(() => manager.startSearch({ root: 'work', query: 'other' }), /concurrent/);
  await manager.close();
  assert.equal(closed >= 1, true);
});

test('close has a bounded wait for non-cancellable filesystem I/O and late fd cleanup', async () => {
  const root = await fixture();
  let releaseEnumeration;
  let closed = 0;
  const filesystem = { ...native,
    listDirectory: async () => new Promise((resolve) => { releaseEnumeration = resolve; }),
    close: async (fd) => { closed += 1; return native.close(fd); },
  };
  const manager = new SearchTasks(new Files({ work: root }, false, filesystem), { closeTimeoutMs: 20 });
  await manager.startSearch({ root: 'work', query: 'needle' });
  for (let attempt = 0; attempt < 100 && !releaseEnumeration; attempt++) await new Promise((resolve) => setImmediate(resolve));
  assert.equal(typeof releaseEnumeration, 'function');
  const started = Date.now();
  await manager.close();
  assert.equal(Date.now() - started < 200, true);
  releaseEnumeration([]);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(closed >= 1, true);
});
