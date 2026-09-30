import { randomBytes } from 'node:crypto';
import { CASEFOLD_EXCEPTIONS } from './casefold.mjs';

const defaults = {
  maxEntries: 1000,
  maxDepth: 30,
  maxVisited: 10000,
  maxResultBytes: 256 * 1024,
  taskTtlMs: 30_000,
  maxConcurrent: 2,
  maxRetainedTasks: 16,
  retentionTtlMs: 300_000,
};

const yieldToLoop = () => new Promise((resolve) => setImmediate(resolve));
const byteLength = (value) => Buffer.byteLength(JSON.stringify(value), 'utf8');
const fold = (value) => Array.from(value, (character) => CASEFOLD_EXCEPTIONS[character] ?? character.toLowerCase()).join('');
const preview = (text, maxCodePoints = 500) => {
  let end = 0; let count = 0;
  for (const character of text) {
    if (count++ >= maxCodePoints) break;
    end += character.length;
  }
  return text.slice(0, end);
};

export class SearchTasks {
  constructor(files, options = {}) {
    if (!files || typeof files.directory !== 'function') throw new TypeError('files must provide directory()');
    this.files = files;
    this.limits = { ...defaults, ...options };
    this.tasks = new Map();
    this.inFlight = new Map();
    this.closed = false;
    this.pruneTimer = setInterval(() => this.#prune(), Math.min(this.limits.retentionTtlMs, 60_000));
    this.pruneTimer.unref?.();
  }

  async startSearch(params = {}) {
    if (this.closed) throw new Error('Search manager is closed');
    this.#prune();
    const { root, path = '.', query, content = false, pattern = 'literal' } = params;
    if (typeof root !== 'string' || typeof query !== 'string' || !query || query.length > 512)
      throw new Error('query must be 1..512 characters');
    if (pattern !== 'literal') throw new Error('Only literal search patterns are supported');
    if (typeof content !== 'boolean') throw new Error('content must be boolean');
    if (!Number.isInteger(this.limits.maxEntries) || this.limits.maxEntries < 1 ||
        !Number.isInteger(this.limits.maxDepth) || this.limits.maxDepth < 0 ||
        !Number.isInteger(this.limits.maxVisited) || this.limits.maxVisited < 1 ||
        !Number.isInteger(this.limits.maxResultBytes) || this.limits.maxResultBytes < 1 ||
        !Number.isInteger(this.limits.taskTtlMs) || this.limits.taskTtlMs < 1 ||
        !Number.isInteger(this.limits.maxConcurrent) || this.limits.maxConcurrent < 1 ||
        !Number.isInteger(this.limits.maxRetainedTasks) || this.limits.maxRetainedTasks < 1 ||
        !Number.isInteger(this.limits.retentionTtlMs) || this.limits.retentionTtlMs < 1) throw new Error('Invalid search resource limits');
    if (this.inFlight.size >= this.limits.maxConcurrent)
      throw new Error('Maximum concurrent searches reached');
    // Validate root/path before creating externally visible task state.
    if (typeof this.files.parts === 'function') this.files.parts(root, path);
    this.#reserveRetentionSlot();
    const searchId = randomBytes(16).toString('hex');
    const task = { search_id: searchId, root, path, query, content, pattern, status: 'queued',
      results: [], resultBytes: 0, visited: 0, skipped: 0, truncated: false, createdAt: Date.now(),
      updatedAt: Date.now(), controller: new AbortController(), fds: new Set() };
    this.tasks.set(searchId, task);
    task.timer = setTimeout(() => { if (['queued', 'running'].includes(task.status)) { task.status = 'expired'; task.finishedAt = Date.now(); task.controller.abort(); this.#prune(); } }, this.limits.taskTtlMs);
    task.promise = this.#run(task).catch((error) => {
      if (task.status === 'stopped' || task.status === 'expired') return;
      task.status = 'failed'; task.finishedAt = Date.now(); task.error = String(error?.message || error); this.#scheduleRetention(task);
    });
    this.inFlight.set(searchId, task.promise);
    return { search_id: searchId, status: task.status, offset: 0, next_offset: 0 };
  }

  start_search(params = {}) { return this.startSearch(params); }

  async getSearchResults(params = {}) {
    this.#prune();
    const task = this.#task(params.search_id);
    const offset = params.offset ?? 0;
    const limit = params.limit ?? 100;
    if (!Number.isInteger(offset) || offset < 0 || !Number.isInteger(limit) || limit < 1 || limit > 200)
      throw new Error('Invalid offset/limit');
    if (offset > task.results.length) throw new Error('offset is beyond results currently available');
    const results = task.results.slice(offset, offset + limit);
    const next = offset + results.length < task.results.length ? offset + results.length :
      ['queued', 'running'].includes(task.status) ? offset + results.length : null;
    return { search_id: task.search_id, status: task.status, results, total: task.results.length,
      offset, next_offset: next, truncated: task.truncated, visited: task.visited,
      skipped: task.skipped, ...(task.truncated_reason ? { truncated_reason: task.truncated_reason } : {}),
      ...(task.error ? { error: task.error } : {}) };
  }

  get_search_results(params = {}) { return this.getSearchResults(params); }

  async stopSearch(params = {}) {
    this.#prune();
    const task = this.#task(params.search_id);
    if (['queued', 'running'].includes(task.status)) { task.status = 'stopped'; task.finishedAt = Date.now(); clearTimeout(task.timer); task.controller.abort(); this.#prune(); }
    return { search_id: task.search_id, status: task.status };
  }

  stop_search(params = {}) { return this.stopSearch(params); }

  listSearches() {
    this.#prune();
    return { searches: [...this.tasks.values()].map((task) => ({ search_id: task.search_id,
      status: task.status, total: task.results.length, visited: task.visited,
      skipped: task.skipped, truncated: task.truncated,
      ...(task.truncated_reason ? { truncated_reason: task.truncated_reason } : {}),
      created_at: task.createdAt, updated_at: task.updatedAt })) };
  }

  list_searches() { return this.listSearches(); }

  async close() {
    this.closed = true;
    clearInterval(this.pruneTimer);
    for (const task of this.tasks.values()) { clearTimeout(task.timer); clearTimeout(task.retentionTimer); if (['queued', 'running'].includes(task.status)) { task.status = 'stopped'; task.controller.abort(); } }
    await Promise.allSettled([...this.inFlight.values()]);
    this.tasks.clear();
  }

  #task(searchId) {
    this.#prune();
    const task = this.tasks.get(searchId);
    if (!task) throw new Error('Unknown search_id');
    return task;
  }

  #prune() {
    const now = Date.now();
    for (const [id, task] of this.tasks) {
      if (!['queued', 'running'].includes(task.status) && task.finishedAt && now - task.finishedAt >= this.limits.retentionTtlMs) {
        clearTimeout(task.timer); clearTimeout(task.retentionTimer); this.tasks.delete(id);
      }
    }
    const retained = [...this.tasks.values()].filter((task) => !this.inFlight.has(task.search_id))
      .sort((a, b) => (a.finishedAt || a.createdAt) - (b.finishedAt || b.createdAt));
    while (this.tasks.size > this.limits.maxRetainedTasks && retained.length) {
      const task = retained.shift(); clearTimeout(task.timer); clearTimeout(task.retentionTimer); this.tasks.delete(task.search_id);
    }
  }

  #reserveRetentionSlot() {
    const retained = [...this.tasks.values()].filter((task) => !this.inFlight.has(task.search_id))
      .sort((a, b) => (a.finishedAt || a.createdAt) - (b.finishedAt || b.createdAt));
    if (this.tasks.size >= this.limits.maxRetainedTasks) {
      if (!retained.length) throw new Error('Maximum retained search tasks reached');
      const task = retained[0]; clearTimeout(task.timer); clearTimeout(task.retentionTimer); this.tasks.delete(task.search_id);
    }
  }

  #scheduleRetention(task) {
    if (this.closed || task.retentionTimer || this.inFlight.has(task.search_id)) return;
    task.retentionTimer = setTimeout(() => {
      if (!this.inFlight.has(task.search_id)) this.tasks.delete(task.search_id);
    }, this.limits.retentionTtlMs);
    task.retentionTimer.unref?.();
  }

  #add(task, result) {
    const bytes = byteLength(result);
    if (task.results.length >= this.limits.maxEntries || task.resultBytes + bytes > this.limits.maxResultBytes) {
      task.truncated = true; task.truncated_reason = task.results.length >= this.limits.maxEntries ? 'max_entries' : 'max_result_bytes'; task.budgetStop = true;
      return false;
    }
    task.results.push(result); task.resultBytes += bytes; task.updatedAt = Date.now();
    return true;
  }

  async #run(task) {
    let rootfd;
    try {
      if (task.controller.signal.aborted || task.status === 'stopped') return;
      task.status = 'running';
      rootfd = await this.files.directory(task.root, task.path);
      task.fds.add(rootfd);
      await this.#walk(task, rootfd, task.path === '.' ? '.' : task.path, 0);
      if (task.status === 'running') { task.status = 'completed'; task.finishedAt = Date.now(); }
    }
    finally {
      clearTimeout(task.timer);
      for (const fd of task.fds) await this.files.close(fd).catch(() => {});
      task.fds.clear();
      this.inFlight.delete(task.search_id);
      if (!['queued', 'running'].includes(task.status)) { task.finishedAt ||= Date.now(); this.#scheduleRetention(task); this.#prune(); }
    }
  }

  async #walk(task, fd, prefix, depth) {
    if (task.controller.signal.aborted || task.status !== 'running') return;
    if (depth > this.limits.maxDepth) { task.truncated = true; task.truncated_reason = 'max_depth'; return; }
    let entries;
    try { entries = await this.files.filesystem.listDirectory(fd); } catch { task.truncated = true; task.truncated_reason = 'directory_error'; task.skipped += 1; return; }
    entries.sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
    for (const entry of entries) {
      if (task.controller.signal.aborted || task.status !== 'running' || task.budgetStop) return;
      task.visited += 1;
      if (task.visited > this.limits.maxVisited) { task.truncated = true; task.truncated_reason = 'max_visited'; return; }
      const rel = `${prefix === '.' ? '' : `${prefix}/`}${entry.name}`;
      if (entry.type !== 'symlink' && !task.content && fold(entry.name).includes(fold(task.query))) this.#add(task, { path: rel });
      if (entry.type === 'directory') {
        let child;
        try { child = await this.files.filesystem.dupDirChild(fd, entry.name); task.fds.add(child); await this.#walk(task, child, rel, depth + 1); }
          catch { task.truncated = true; task.truncated_reason = 'directory_error'; task.skipped += 1; }
        finally { if (child !== undefined) { task.fds.delete(child); await this.files.close(child).catch(() => {}); } }
      } else if (task.content && entry.type === 'file') {
        try {
          const data = await this.files._read(fd, entry.name);
          const text = new TextDecoder('utf-8', { fatal: true }).decode(data);
          for (const [index, line] of text.split(/\r?\n/).entries()) {
            if (line.includes(task.query) && !this.#add(task, { path: rel, line: index + 1, text: preview(line) })) break;
          }
        } catch { task.skipped += 1; }
      }
      await yieldToLoop();
    }
  }
}

export const createSearchTasks = (files, options) => new SearchTasks(files, options);
export default SearchTasks;
