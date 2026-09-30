import fsSync from 'node:fs';
import * as fs from 'node:fs/promises';
import path from 'node:path';
import { randomBytes } from 'node:crypto';

const ownerName = 'owner.json';
const currentUid = () => typeof process.getuid === 'function' ? process.getuid() : null;

function lockPath(config) { return `${config}.node-lock`; }

function ownerError(lock) {
  return new Error(`设备配置锁已存在：${lock}；请先停止旧 Agent 并人工清理 stale 锁`);
}

export async function acquireProfileLocks(configPaths) {
  const paths = [...new Set(configPaths)].sort();
  const held = [];
  try {
    for (const config of paths) {
      const lock = lockPath(config);
      await fs.mkdir(path.dirname(lock), { recursive: true, mode: 0o700 });
      const owner = { pid: process.pid, nonce: randomBytes(32).toString('base64url') };
      try {
        fsSync.mkdirSync(lock, 0o700);
      } catch (error) {
        if (error.code === 'EEXIST') throw ownerError(lock);
        throw error;
      }
      // The directory is ours only after this lstat.  If owner creation fails,
      // leave the lock in place for explicit manual cleanup; never recursively
      // remove a path that may have been replaced by another process.
      const lockStat = fsSync.lstatSync(lock);
      if (!lockStat.isDirectory() || lockStat.isSymbolicLink()) throw ownerError(lock);
      try {
        const ownerFile = path.join(lock, ownerName);
        const fd = fsSync.openSync(ownerFile, fsSync.constants.O_WRONLY | fsSync.constants.O_CREAT | fsSync.constants.O_EXCL | fsSync.constants.O_NOFOLLOW, 0o600);
        try { fsSync.writeFileSync(fd, JSON.stringify(owner) + '\n', { encoding: 'utf8' }); }
        finally { fsSync.closeSync(fd); }
        fsSync.chmodSync(ownerFile, 0o600);
      } catch (error) {
        throw error;
      }
      held.push({ lock, owner, dev: lockStat.dev, ino: lockStat.ino });
    }
  } catch (error) {
    await releaseProfileLocks({ held });
    throw error;
  }
  return { held, release: () => releaseProfileLocks({ held }) };
}

export async function releaseProfileLocks(handle) {
  const held = handle?.held || [];
  for (const item of [...held].reverse()) {
    try {
      const stat = await fs.lstat(item.lock);
      if (!stat.isDirectory() || stat.dev !== item.dev || stat.ino !== item.ino) continue;
      const ownerFile = path.join(item.lock, ownerName);
      const ownerHandle = await fs.open(ownerFile, fsSync.constants.O_RDONLY | fsSync.constants.O_NOFOLLOW);
      let ownerStat;
      let ownerText;
      try {
        ownerStat = await ownerHandle.stat();
        if (!ownerStat.isFile() || (currentUid() !== null && ownerStat.uid !== currentUid()) || (ownerStat.mode & 0o077)) continue;
        ownerText = await ownerHandle.readFile('utf8');
      } finally { await ownerHandle.close(); }
      const owner = JSON.parse(ownerText);
      if (owner.pid !== item.owner.pid || owner.nonce !== item.owner.nonce) continue;
      await fs.unlink(ownerFile);
      await fs.rmdir(item.lock);
    } catch { /* Never remove a lock we can no longer prove is ours. */ }
  }
}

export { lockPath };
