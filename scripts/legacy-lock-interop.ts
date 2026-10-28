// Test fixture: bundle the current legacy implementation and exercise it in a
// separate Node process. This file is not part of the native Agent runtime.
import { createInterface } from 'node:readline';
import { acquireProfileLocks, reclaimProfileLocks } from '../src/profile-lock.js';
import { ensurePrivatePath } from '../src/platform.js';

const [action, path] = process.argv.slice(2);
if (!action || !path) {
  throw new Error('The fixture requires an action and path');
}

function expectedLockRejection(error: unknown): boolean {
  return (
    error instanceof Error &&
    error.message.startsWith('The device configuration lock already exists or is being recovered:')
  );
}

if (action === 'prepare') {
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
