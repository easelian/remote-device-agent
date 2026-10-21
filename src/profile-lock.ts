import fsSync, { type Stats } from 'node:fs';
import { errorCode, isRecord, isSafeInteger } from './boundary.js';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { assertPrivatePath, secureNewPath } from './platform.js';

interface LockOwner {
  pid: number;
  nonce: string;
}
interface HeldProfileLock {
  lock: string;
  owner: LockOwner;
  dev: number;
  ino: number;
}
interface ReclaimGuard {
  path: string;
  dev: number;
  ino: number;
}
export interface ProfileLockHandle {
  held: HeldProfileLock[];
  release(): Promise<void>;
}

function isLockOwner(value: unknown): value is LockOwner {
  return (
    isRecord(value) &&
    isSafeInteger(value.pid) &&
    value.pid >= 1 &&
    typeof value.nonce === 'string' &&
    value.nonce.length >= 32
  );
}

const ownerName = 'owner.json';
const currentUid = () => (typeof process.getuid === 'function' ? process.getuid() : null);
const reclaimName = (lock: string) => `${lock}.reclaiming`;

function lockPath(config: string) {
  return `${config}.node-lock`;
}

function ownerError(lock: string) {
  return new Error(
    `设备配置锁已存在或正在恢复：${lock}；如确认旧 Agent 已停止，请手动清理（manual cleanup）或使用 --recover-lock`,
  );
}

function assertPrivateDirectory(filename: string, stat: Stats, _label: string) {
  try {
    assertPrivatePath(filename, true);
  } catch {
    throw ownerError(filename);
  }
  if (!stat.isDirectory() || stat.isSymbolicLink()) {
    throw ownerError(filename);
  }
  return stat;
}

function readOwner(lock: string) {
  const ownerFile = path.join(lock, ownerName);
  assertPrivatePath(ownerFile, false);
  const fd = fsSync.openSync(
    ownerFile,
    fsSync.constants.O_RDONLY | (fsSync.constants.O_NOFOLLOW || 0),
  );
  try {
    const stat = fsSync.fstatSync(fd);
    if (
      !stat.isFile() ||
      (currentUid() !== null && stat.uid !== currentUid()) ||
      (process.platform !== 'win32' && stat.mode & 0o077)
    ) {
      throw ownerError(lock);
    }
    const owner: unknown = JSON.parse(fsSync.readFileSync(fd, 'utf8'));
    if (!isLockOwner(owner)) {
      throw ownerError(lock);
    }
    return { owner, stat };
  } finally {
    fsSync.closeSync(fd);
  }
}

function ownerIsDead(pid: number) {
  try {
    process.kill(pid, 0);
    return false;
  } catch (error) {
    // EPERM means a process exists but is not inspectable.  Only ESRCH proves
    // that the owner is gone; treating other errors as stale could break the
    // single-writer guarantee.
    if (errorCode(error) === 'ESRCH') {
      return true;
    }
    throw error;
  }
}

function guardPath(lock: string) {
  return reclaimName(lock);
}

function makeGuard(lock: string) {
  const guard = guardPath(lock);
  try {
    fsSync.mkdirSync(guard, 0o700);
    secureNewPath(guard, true);
  } catch (error) {
    if (errorCode(error) === 'EEXIST') {
      throw ownerError(lock);
    }
    throw error;
  }
  try {
    const stat = fsSync.lstatSync(guard);
    assertPrivateDirectory(guard, stat, 'reclaim guard');
    return { path: guard, dev: stat.dev, ino: stat.ino };
  } catch (error) {
    try {
      fsSync.rmdirSync(guard);
    } catch {}
    throw error;
  }
}

function releaseGuard(guard: ReclaimGuard) {
  try {
    const stat = fsSync.lstatSync(guard.path);
    if (stat.isDirectory() && stat.dev === guard.dev && stat.ino === guard.ino) {
      fsSync.rmdirSync(guard.path);
    }
  } catch {}
}

export async function acquireProfileLocks(
  configPaths: readonly string[],
): Promise<ProfileLockHandle> {
  const paths = [...new Set(configPaths)].sort();
  const held: HeldProfileLock[] = [];
  try {
    for (const config of paths) {
      const lock = lockPath(config);
      await fs.mkdir(path.dirname(lock), { recursive: true, mode: 0o700 });
      const owner = { pid: process.pid, nonce: randomBytes(32).toString('base64url') };
      // The reclaim guard is the shared short mutex.  A pre-check followed by
      // mkdir(lock) is insufficient: a reclaimer could rename a stale lock
      // between those two operations and an acquire could then move into the
      // path while the reclaimer still believed it owned the old inode.
      const guard = makeGuard(lock);
      try {
        try {
          fsSync.mkdirSync(lock, 0o700);
          secureNewPath(lock, true);
        } catch (error) {
          if (errorCode(error) === 'EEXIST') {
            throw ownerError(lock);
          }
          throw error;
        }
        const lockStat = fsSync.lstatSync(lock);
        assertPrivateDirectory(lock, lockStat, 'profile lock');
        const ownerFile = path.join(lock, ownerName);
        const fd = fsSync.openSync(
          ownerFile,
          fsSync.constants.O_WRONLY |
            fsSync.constants.O_CREAT |
            fsSync.constants.O_EXCL |
            (fsSync.constants.O_NOFOLLOW || 0),
          0o600,
        );
        try {
          secureNewPath(ownerFile, false);
          fsSync.writeFileSync(fd, JSON.stringify(owner) + '\n', { encoding: 'utf8' });
        } finally {
          fsSync.closeSync(fd);
        }
        if (process.platform !== 'win32') {
          fsSync.chmodSync(ownerFile, 0o600);
        }
        held.push({ lock, owner, dev: lockStat.dev, ino: lockStat.ino });
      } finally {
        releaseGuard(guard);
      }
    }
  } catch (error) {
    await releaseProfileLocks({ held });
    throw error;
  }
  return { held, release: () => releaseProfileLocks({ held }) };
}

/**
 * Explicitly reclaim locks left by a crashed Agent.  This is intentionally
 * separate from acquireProfileLocks: a normal startup never kills or guesses
 * about another process.  All participating acquirers honor the sibling
 * reclaim guard, which serializes two reclaimers and prevents a fresh acquire
 * from racing the rename.  A crashed reclaimer leaves the guard behind and
 * therefore fails closed until it is manually inspected.
 */
export async function reclaimProfileLocks(configPaths: readonly string[]) {
  const paths = [...new Set(configPaths)].sort();
  const guards: ReclaimGuard[] = [];
  try {
    for (const config of paths) {
      const lock = lockPath(config);
      await fs.mkdir(path.dirname(lock), { recursive: true, mode: 0o700 });
      guards.push(makeGuard(lock));
    }
    const reclaimed = [];
    for (const config of paths) {
      const lock = lockPath(config);
      let lockStat;
      try {
        lockStat = fsSync.lstatSync(lock);
      } catch (error) {
        if (errorCode(error) === 'ENOENT') {
          continue;
        }
        throw error;
      }
      assertPrivateDirectory(lock, lockStat, 'profile lock');
      const { owner } = readOwner(lock);
      if (!ownerIsDead(owner.pid)) {
        throw ownerError(lock);
      }

      // rename is atomic with respect to mkdir(lock).  The guard blocks all
      // cooperative acquire/reclaim callers until the quarantine is cleaned.
      const quarantine = `${lock}.stale-${randomBytes(16).toString('hex')}`;
      fsSync.renameSync(lock, quarantine);
      try {
        const moved = fsSync.lstatSync(quarantine);
        if (moved.dev !== lockStat.dev || moved.ino !== lockStat.ino) {
          throw ownerError(lock);
        }
        fsSync.unlinkSync(path.join(quarantine, ownerName));
        fsSync.rmdirSync(quarantine);
      } catch (error) {
        // The original lock path is already free; leave a non-active
        // quarantine for manual cleanup if its contents cannot be removed.
        throw error;
      }
      reclaimed.push(lock);
    }
    return { reclaimed };
  } finally {
    for (const guard of guards.reverse()) {
      releaseGuard(guard);
    }
  }
}

export async function releaseProfileLocks(
  handle: Pick<ProfileLockHandle, 'held'> | null | undefined,
) {
  const held = handle?.held || [];
  for (const item of [...held].reverse()) {
    try {
      const stat = await fs.lstat(item.lock);
      if (!stat.isDirectory() || stat.dev !== item.dev || stat.ino !== item.ino) {
        continue;
      }
      const ownerFile = path.join(item.lock, ownerName);
      assertPrivatePath(ownerFile, false);
      const ownerHandle = await fs.open(
        ownerFile,
        fsSync.constants.O_RDONLY | (fsSync.constants.O_NOFOLLOW || 0),
      );
      let ownerStat;
      let ownerText;
      try {
        ownerStat = await ownerHandle.stat();
        if (
          !ownerStat.isFile() ||
          (currentUid() !== null && ownerStat.uid !== currentUid()) ||
          (process.platform !== 'win32' && ownerStat.mode & 0o077)
        ) {
          continue;
        }
        ownerText = await ownerHandle.readFile('utf8');
      } finally {
        await ownerHandle.close();
      }
      const owner: unknown = JSON.parse(ownerText);
      if (!isLockOwner(owner)) {
        continue;
      }
      if (owner.pid !== item.owner.pid || owner.nonce !== item.owner.nonce) {
        continue;
      }
      await fs.unlink(ownerFile);
      await fs.rmdir(item.lock);
    } catch {
      /* Never remove a lock we can no longer prove is ours. */
    }
  }
}

export { lockPath };
