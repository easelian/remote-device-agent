// Test-only access to the legacy Agent's actual file metadata implementation.
import { Files } from '../src/files.js';

const root = process.argv[2];
if (!root) {
  throw new Error('The fixture requires a file root');
}
const files = new Files({ work: root }, false);
const writable = new Files({ work: root }, true);
const params = { root: 'work', path: 'file' };
const info = await files.fileInfo(params);
const directory_info = await files.fileInfo({ root: 'work', path: 'directory' });
const root_info = await files.fileInfo({ root: 'work', path: '.' });
const list = await files.list({ root: 'work' });
const chunk = await files.readFileChunk(params);
const batch = await files.readMultipleFiles({ root: 'work', paths: ['file'] });
const list_large_offset = await files.list({ root: 'work', offset: 1e100 });
async function rejected(operation: Promise<unknown>): Promise<string> {
  try {
    await operation;
    throw new Error('Fixture expected a rejection');
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  }
}
const invalid = {
  read_offset: await rejected(files.read({ ...params, offset: -1 })),
  read_length: await rejected(files.read({ ...params, length: 0 })),
  chunk_encoding: await rejected(files.readFileChunk({ ...params, encoding: 0 })),
  chunk_from_end: await rejected(files.readFileChunk({ ...params, from_end: null })),
  root: await rejected(files.read({ root: null, path: null })),
  path: await rejected(files.read({ root: 'work', path: null })),
  chunk_empty: await rejected(files.readFileChunk({ root: 'work', path: '.' })),
  batch_range: await files.readMultipleFiles({ root: 'work', paths: ['file'], length: 0 }),
  batch_empty: await files.readMultipleFiles({ root: 'work', paths: ['.'] }),
  list_offset: await rejected(files.list({ root: 'work', offset: -1 })),
  write_disabled: await rejected(files.write({ root: null, path: null, content: null })),
  write_content: await rejected(writable.write({ ...params, content: null })),
  write_expected: await rejected(
    writable.write({ ...params, content: 'new', expected_sha256: null }),
  ),
  edit_empty: await rejected(writable.edit({ ...params, old_text: 0, new_text: 'new' })),
  edit_type: await rejected(writable.edit({ ...params, old_text: {}, new_text: 'new' })),
  mkdir_path: await rejected(writable.mkdir({ root: 'work', path: null })),
  move_type: await rejected(writable.moveFile({ root: 'work', source: null, destination: 'file' })),
  search_query: await rejected(files.search({ root: 'work', query: null })),
};
console.log(
  JSON.stringify({
    list,
    info,
    directory_info,
    root_info,
    chunk,
    batch,
    list_large_offset,
    invalid,
  }),
);
