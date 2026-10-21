import type { Awaitable } from './boundary.js';
import type { DirectoryEntry, DirectoryHandle } from './posix-fs.js';

export interface FileMetadata {
  type: DirectoryEntry['type'];
  size_bytes: number;
  mode: number;
  mtime_ms: number;
  dev: number;
  ino: number;
}

export interface FileChunk extends Omit<FileMetadata, 'type'> {
  bytes: Buffer;
  tail: Buffer;
  next_offset: number;
  truncated: boolean;
}

export interface DirectoryOptions {
  maxEntries?: number;
  signal?: AbortSignal;
  allowTruncated?: boolean;
}

export interface DirectoryListing {
  entries: DirectoryEntry[];
  truncated?: boolean;
  reason?: string;
}

/** Native operations and fault-injected adapters may complete synchronously or asynchronously. */
export interface BoundedFilesystem {
  openDirectory(baseAbsolute: string, relative?: string): Awaitable<DirectoryHandle>;
  close(handle: DirectoryHandle): Awaitable<void>;
  readFile(parent: DirectoryHandle, name: string, maxBytes: number): Awaitable<Uint8Array>;
  statAt(parent: DirectoryHandle, name: string): Awaitable<FileMetadata>;
  statDirectory(handle: DirectoryHandle): Awaitable<FileMetadata>;
  readFileChunk(
    parent: DirectoryHandle,
    name: string,
    offset: number,
    length: number,
    maxBytes: number,
  ): Awaitable<FileChunk>;
  listDirectory(parent: DirectoryHandle): Awaitable<DirectoryEntry[]>;
  listDirectory(
    parent: DirectoryHandle,
    options: DirectoryOptions,
  ): Awaitable<DirectoryEntry[] | DirectoryListing>;
  writeAtomic(
    parent: DirectoryHandle,
    name: string,
    bytes: Uint8Array,
    mode: number,
    createOnly: boolean,
  ): Awaitable<void>;
  moveAtomic(
    sourceParent: DirectoryHandle,
    sourceName: string,
    destinationParent: DirectoryHandle,
    destinationName: string,
  ): Awaitable<void>;
  mkdirAt(parent: DirectoryHandle, name: string): Awaitable<void>;
  dupDirChild(parent: DirectoryHandle, name: string): Awaitable<DirectoryHandle>;
}
