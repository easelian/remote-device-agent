// Test fixture: bundle the current legacy implementation and exercise it in a
// separate Node process. This file is not part of the native Agent runtime.
import { createInterface } from 'node:readline';
import { realpathSync } from 'node:fs';
import childProcess from 'node:child_process';
import { syncBuiltinESMExports } from 'node:module';
import { basename } from 'node:path';
import { acquireProfileLocks, reclaimProfileLocks } from '../src/profile-lock.js';
import { ensurePrivatePath } from '../src/platform.js';

const [action, path] = process.argv.slice(2);
if (!action || !path) {
  throw new Error('The fixture requires an action and path');
}

// Observe the real legacy ACL subprocesses without changing their results or
// deadlines. In particular, one lock operation can invoke several bounded
// PowerShell commands. Do not print their arguments, environment or output.
if (process.env.RDH_TEST_LEGACY_LOCK_TRACE === '1') {
  const original = childProcess.spawnSync;
  let sequence = 0;
  let aclFailures = 0;
  childProcess.spawnSync = ((...args: Parameters<typeof original>) => {
    const id = ++sequence;
    const started = performance.now();
    const detail = {
      fixture: action,
      id,
      executable: basename(args[0]),
      acl_action: args[2]?.env?.RDH_PRIVATE_ACTION,
    };
    console.error(JSON.stringify({ ...detail, phase: 'begin' }));
    const result = original(...args);
    if (
      detail.acl_action &&
      (result.error || result.status !== 0 || String(result.stdout).trim() !== 'RDH_PRIVATE_OK')
    ) {
      aclFailures += 1;
    }
    console.error(
      JSON.stringify({
        ...detail,
        phase: 'end',
        elapsed_ms: Math.round(performance.now() - started),
        status: result.status,
        signal: result.signal,
        error: result.error?.name,
        error_code: result.error && 'code' in result.error ? result.error.code : undefined,
      }),
    );
    return result;
  }) as typeof childProcess.spawnSync;
  syncBuiltinESMExports();
  process.on('beforeExit', () => {
    if (aclFailures !== 0) {
      // Legacy private-directory errors can be mapped to "lock exists". Such
      // a failure cannot count as proof of lock exclusion in this fixture.
      console.error(`Legacy ACL subprocess failures: ${aclFailures}`);
      process.exitCode = 1;
    }
  });
}

function expectedLockRejection(error: unknown): boolean {
  return (
    error instanceof Error &&
    error.message.startsWith('The device configuration lock already exists or is being recovered:')
  );
}

if (action === 'canonical') {
  console.log(JSON.stringify(realpathSync.native(path)));
} else if (action === 'prepare') {
  ensurePrivatePath(path, true);
  console.log('prepared');
} else if (action === 'hold') {
  const held = await acquireProfileLocks([path]);
  console.log('held');
  const input = createInterface({ input: process.stdin });
  let release = false;
  for await (const line of input) {
    if (line === 'release') {
      release = true;
      break;
    }
  }
  input.close();
  if (!release) {
    throw new Error('The fixture did not receive a release request');
  }
  await held.release();
  console.log('released');
} else if (action === 'acquire') {
  try {
    const held = await acquireProfileLocks([path]);
    await held.release();
    console.log('acquired');
  } catch (error) {
    if (!expectedLockRejection(error)) {
      throw error;
    }
    console.log('blocked');
  }
} else if (action === 'recover') {
  try {
    await reclaimProfileLocks([path]);
    console.log('recovered');
  } catch (error) {
    if (!expectedLockRejection(error)) {
      throw error;
    }
    console.log('blocked');
  }
} else {
  throw new Error('Unknown fixture action');
}
