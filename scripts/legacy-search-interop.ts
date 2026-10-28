// Test-only access to the legacy Agent's actual background-search implementation.
import { Files } from '../src/files.js';
import { SearchTasks } from '../src/searches.js';

const root = process.argv[2];
if (!root) {
  throw new Error('The fixture requires a file root');
}
const manager = new SearchTasks(new Files({ work: root }, false));

async function rejected(operation: Promise<unknown>): Promise<string> {
  try {
    await operation;
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
  throw new Error('Fixture expected a rejection');
}

async function complete(searchId: string) {
  for (let attempt = 0; attempt < 600; attempt++) {
    const result = await manager.getSearchResults({ search_id: searchId });
    if (!['queued', 'running'].includes(result.status)) {
      const { search_id: _id, ...rest } = result;
      return rest;
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error('Fixture search did not finish');
}

async function retentionExample() {
  let release = () => {};
  const slowGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const controlled = new SearchTasks<string>({
    parts: () => [],
    directory: async (root) => {
      if (root === 'slow') {
        await slowGate;
      }
      return root;
    },
    close: async () => {},
    _read: async () => new Uint8Array(),
    filesystem: {
      listDirectory: () => [],
      dupDirChild: async (handle) => handle,
    },
  });
  async function waitFor(searchId: string) {
    for (let attempt = 0; attempt < 600; attempt++) {
      const result = await controlled.getSearchResults({ search_id: searchId });
      if (result.status === 'completed') {
        return;
      }
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    throw new Error('Controlled search did not finish');
  }
  try {
    const slow = await controlled.startSearch({ root: 'slow', query: 'x' });
    const fast = await controlled.startSearch({ root: 'fast', query: 'x' });
    await waitFor(fast.search_id);
    await new Promise((resolve) => setTimeout(resolve, 5));
    release();
    await waitFor(slow.search_id);
    for (let index = 0; index < 14; index++) {
      const task = await controlled.startSearch({ root: 'other', query: 'x' });
      await waitFor(task.search_id);
    }
    const replacement = await controlled.startSearch({ root: 'other', query: 'x' });
    await waitFor(replacement.search_id);
    const ids = controlled.listSearches().searches.map((task) => task.search_id);
    return {
      slow_retained: ids.includes(slow.search_id),
      fast_retained: ids.includes(fast.search_id),
      total: ids.length,
    };
  } finally {
    release();
    await controlled.close();
  }
}

try {
  const invalid = {
    input: await rejected(manager.startSearch(null)),
    path_type: await rejected(manager.startSearch({ root: 'work', path: null, query: 'x' })),
    query_before_pattern: await rejected(
      manager.startSearch({ root: 'work', query: '', pattern: 0 }),
    ),
    pattern_before_content: await rejected(
      manager.startSearch({ root: 'work', query: 'x', pattern: 0, content: 0 }),
    ),
    content: await rejected(manager.startSearch({ root: 'work', query: 'x', content: 0 })),
    root_before_path: await rejected(
      manager.startSearch({ root: 'missing', path: '../x', query: 'x' }),
    ),
    parent_path: await rejected(manager.startSearch({ root: 'work', path: '../x', query: 'x' })),
  };
  const name = await manager.startSearch({ root: 'work', query: 'strasse' });
  if (!['queued', 'running'].includes(name.status)) {
    throw new Error('Search did not enter an active state');
  }
  const nameResult = await complete(name.search_id);
  const page = await manager.getSearchResults({ search_id: name.search_id, offset: 1, limit: 1 });
  const { search_id: _pageId, ...namePage } = page;
  const pageErrors = {
    negative: await rejected(manager.getSearchResults({ search_id: name.search_id, offset: -1 })),
    fractional: await rejected(
      manager.getSearchResults({ search_id: name.search_id, offset: 0.5 }),
    ),
    large: await rejected(manager.getSearchResults({ search_id: name.search_id, offset: 1e100 })),
    zero_limit: await rejected(manager.getSearchResults({ search_id: name.search_id, limit: 0 })),
    both_invalid: await rejected(
      manager.getSearchResults({ search_id: name.search_id, offset: 1e100, limit: 0 }),
    ),
  };
  const content = await manager.startSearch({ root: 'work', query: 'needle', content: true });
  const contentResult = await complete(content.search_id);
  const emptyName = await manager.startSearch({ root: 'work', path: '', query: 'strasse' });
  const emptyNameResult = await complete(emptyName.search_id);
  const emptyContent = await manager.startSearch({
    root: 'work',
    path: '',
    query: 'needle',
    content: true,
  });
  const emptyContentResult = await complete(emptyContent.search_id);
  const nested = await manager.startSearch({
    root: 'work',
    path: 'deep/',
    query: 'needle',
    content: true,
  });
  const nestedResult = await complete(nested.search_id);
  const missing = await manager.startSearch({ root: 'work', path: 'missing', query: 'x' });
  const missingResult = await complete(missing.search_id);
  // Native errors deliberately omit absolute paths; compare presence, not OS error text.
  const { error, ...missingFields } = missingResult;
  const absent = { ...missingFields, has_error: typeof error === 'string' && error.length > 0 };
  await manager.close();
  const closed = await rejected(manager.startSearch({ root: 'work', query: '' }));
  const retention = await retentionExample();
  console.log(
    JSON.stringify({
      invalid,
      nameResult,
      namePage,
      pageErrors,
      contentResult,
      emptyNameResult,
      emptyContentResult,
      nestedResult,
      absent,
      closed,
      retention,
    }),
  );
} finally {
  await manager.close();
}
