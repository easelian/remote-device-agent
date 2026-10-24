import test from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { configPath, configPaths, localRoots, readConfig, writeConfig } from '../.build/config.js';
import { connectDevice, pairDevice } from '../.build/connect.js';
import { acquireProfileLocks } from '../.build/profile-lock.js';
import { PUBLIC_SERVER, MCP_SERVER } from '../.build/server-selection.js';

const tmp = async () => fs.mkdtemp(path.join(os.tmpdir(), 'rdh-node-agent-'));
const locks = { acquire: async () => ({ release: async () => {} }) };

test('route changes reuse a private binding and lock both deployment origins', async () => {
  const base = await tmp();
  const previous = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = base;
  try {
    const destination = configPath(PUBLIC_SERVER);
    await fs.mkdir(path.dirname(destination), { recursive: true });
    await writeConfig(destination, {
      gateway_url: PUBLIC_SERVER,
      device_id: 'existing',
      token: 'x'.repeat(32),
      roots: { work: base },
    });
    let acquired;
    for (const server of [MCP_SERVER, PUBLIC_SERVER]) {
      await connectDevice(
        { server },
        {
          locks: {
            acquire: async (targets) => {
              acquired = targets;
              return { release: async () => {} };
            },
          },
          runAgent: async (config) => {
            assert.equal(config.device_id, 'existing');
            assert.equal(config.gateway_url, server);
          },
        },
      );
      assert.deepEqual(
        acquired,
        [...new Set([PUBLIC_SERVER, MCP_SERVER])].flatMap((server) => configPaths(server)),
      );
    }
    assert.equal((await readConfig(destination)).gateway_url, PUBLIC_SERVER);
    await assert.rejects(
      () => connectDevice({ server: 'https://unrelated.test', config: destination }, { locks }),
      /another Hub/,
    );
  } finally {
    if (previous === undefined) {
      delete process.env.XDG_CONFIG_HOME;
    } else {
      process.env.XDG_CONFIG_HOME = previous;
    }
    await fs.rm(base, { recursive: true, force: true });
  }
});

test('config path keeps server hash/profile and reuses legacy files', async () => {
  const base = await tmp();
  const env = { XDG_CONFIG_HOME: base };
  const current = configPath('https://example.test', 'dev', env);
  assert.match(current, /remote-device-hub\/[^/]+-dev\.json$/);
  const legacy = current.replace('/remote-device-hub/', '/remote-device-host/');
  await fs.mkdir(path.dirname(legacy), { recursive: true });
  await fs.writeFile(legacy, '{}');
  assert.equal(configPath('https://example.test', 'dev', env), legacy);
});

test('private config is exclusive, owner checked, and defaults are preserved', async () => {
  const base = await tmp();
  const filename = path.join(base, 'config.json');
  const config = {
    gateway_url: 'https://example.test',
    device_id: 'd',
    token: 'x'.repeat(32),
    roots: { work: base },
  };
  await writeConfig(filename, config);
  assert.equal((await readConfig(filename)).max_processes, 4);
  await assert.rejects(() => writeConfig(filename, config), /EEXIST/);
  await fs.chmod(filename, 0o644);
  await assert.rejects(() => readConfig(filename), /0600/);
});

test('pairing rejects redirected or cross-origin verification links', async () => {
  const original = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response(
      JSON.stringify({
        verification_uri_complete: 'https://evil.test/admin/pair?code=A',
        user_code: 'A',
      }),
      { status: 201 },
    );
  await assert.rejects(
    () => pairDevice('https://example.test', { work: '/tmp' }, 'device', { openBrowser: false }),
    /invalid pairing URL/,
  );
  globalThis.fetch = original;
});

test('direct transport accepts canonical binding URL but keeps polling the chosen transport', async () => {
  const original = globalThis.fetch;
  const calls = [];
  globalThis.fetch = async (url) => {
    calls.push(String(url));
    if (url.pathname === '/api/pair/start') {
      return Response.json(
        {
          verification_uri_complete: `${MCP_SERVER}/admin/pair?code=TEST`,
          user_code: 'TEST',
          device_code: 'pending-test',
          expires_in: 10,
          interval: 3,
        },
        { status: 201 },
      );
    }
    return Response.json({
      config: { gateway_url: MCP_SERVER, device_id: 'paired', roots: { work: '/tmp' } },
    });
  };
  try {
    const config = await pairDevice(PUBLIC_SERVER, { work: '/tmp' }, 'device', {
      openBrowser: false,
    });
    assert.equal(config.gateway_url, MCP_SERVER);
    assert.deepEqual(calls, [`${PUBLIC_SERVER}/api/pair/start`, `${PUBLIC_SERVER}/api/pair/poll`]);
  } finally {
    globalThis.fetch = original;
  }
});

test('connectDevice injects pure Node profile lock and runAgent without Python', async () => {
  const base = await tmp();
  const destination = path.join(base, 'profile.json');
  const config = {
    gateway_url: 'https://example.test',
    device_id: 'd',
    token: 'x'.repeat(32),
    roots: { work: base },
  };
  await writeConfig(destination, config);
  let received;
  const result = await connectDevice(
    { server: 'https://example.test', config: destination },
    {
      locks,
      runAgent: async (value, signal) => {
        received = [value, signal];
        return 17;
      },
    },
  );
  assert.equal(result, 17);
  assert.equal(received[0].device_id, 'd');
  assert.equal(received[1], undefined);
});

test('current config cannot bypass an active legacy profile lock', async () => {
  const base = await tmp();
  const previous = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = base;
  try {
    const env = { XDG_CONFIG_HOME: base };
    const [current, legacy] = configPaths('https://example.test', 'same', env);
    await fs.mkdir(path.dirname(current), { recursive: true });
    await writeConfig(current, {
      gateway_url: 'https://example.test',
      device_id: 'd',
      token: 'x'.repeat(32),
      roots: { work: base },
    });
    const lockingLocks = {
      acquire: async () => {
        throw new Error('Another Agent is already running');
      },
    };
    await assert.rejects(
      () =>
        connectDevice({ server: 'https://example.test', profile: 'same' }, { locks: lockingLocks }),
      /Another Agent/,
    );
  } finally {
    if (previous === undefined) {
      delete process.env.XDG_CONFIG_HOME;
    } else {
      process.env.XDG_CONFIG_HOME = previous;
    }
  }
});

test('legacy config is reused after both profile locks are acquired', async () => {
  const base = await tmp();
  const previous = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = base;
  try {
    const env = { XDG_CONFIG_HOME: base };
    const [current, legacy] = configPaths('https://example.test', 'old', env);
    await fs.mkdir(path.dirname(legacy), { recursive: true });
    await writeConfig(legacy, {
      gateway_url: 'https://example.test',
      device_id: 'legacy',
      token: 'x'.repeat(32),
      roots: { work: base },
    });
    const acquired = [];
    const lockingLocks = {
      acquire: async (paths) => {
        acquired.push(...paths);
        return { release: async () => {} };
      },
    };
    let ran = false;
    await connectDevice(
      { server: 'https://example.test', profile: 'old' },
      {
        locks: lockingLocks,
        runAgent: async (config) => {
          ran = config.device_id === 'legacy';
        },
      },
    );
    assert.equal(ran, true);
    assert.deepEqual(acquired, [current, legacy]);
  } finally {
    if (previous === undefined) {
      delete process.env.XDG_CONFIG_HOME;
    } else {
      process.env.XDG_CONFIG_HOME = previous;
    }
  }
});

test('real Node profile lock rejects duplicate, releases, and fails closed on stale owner', async () => {
  const base = await tmp();
  const config = path.join(base, 'profile.json');
  const first = await acquireProfileLocks([config]);
  await assert.rejects(() => acquireProfileLocks([config]), /configuration lock already exists/);
  await first.release();
  const second = await acquireProfileLocks([config]);
  await second.release();
  await fs.mkdir(`${config}.node-lock`, { recursive: true, mode: 0o700 });
  const staleOwner = JSON.stringify({ pid: 999999999, nonce: 'x'.repeat(43) }) + '\n';
  await fs.writeFile(`${config}.node-lock/owner.json`, staleOwner, { mode: 0o600 });
  await assert.rejects(() => acquireProfileLocks([config]), /--recover-lock/);
  assert.equal(await fs.readFile(`${config}.node-lock/owner.json`, 'utf8'), staleOwner);
  await fs.rm(`${config}.node-lock`, { recursive: true });
});

test('localRoots resolves existing directories', async () => {
  const base = await tmp();
  assert.deepEqual(await localRoots([`work=${base}`]), { work: await fs.realpath(base) });
  await assert.rejects(() => localRoots(['bad name=/tmp']), /Invalid file root name/);
});

test('explicit recovery finishes before acquiring both profile locks', async () => {
  const base = await tmp();
  const destination = path.join(base, 'profile.json');
  await writeConfig(destination, {
    gateway_url: 'https://example.test',
    device_id: 'd',
    token: 'x'.repeat(32),
    roots: { work: base },
  });
  const calls = [];
  await connectDevice(
    { server: 'https://example.test', config: destination, recoverLock: true },
    {
      locks: {
        reclaim: async (targets) => {
          calls.push(['recover', targets]);
        },
        acquire: async (targets) => {
          calls.push(['acquire', targets]);
          return {
            release: async () => {
              calls.push(['release']);
            },
          };
        },
      },
      runAgent: async () => {
        calls.push(['run']);
      },
    },
  );
  assert.deepEqual(calls, [
    ['recover', [destination]],
    ['acquire', [destination]],
    ['run'],
    ['release'],
  ]);
});

test('unfinished operations retain the profile lock until the Agent exits', async () => {
  const base = await tmp();
  const destination = path.join(base, 'profile.json');
  await writeConfig(destination, {
    gateway_url: 'https://example.test',
    device_id: 'd',
    token: 'x'.repeat(32),
    roots: { work: base },
  });
  let released = false;
  await connectDevice(
    { server: 'https://example.test', config: destination },
    {
      locks: {
        acquire: async () => ({
          release: async () => {
            released = true;
          },
        }),
      },
      runAgent: async () => {},
      canReleaseLocks: () => false,
    },
  );
  assert.equal(released, false);
});
