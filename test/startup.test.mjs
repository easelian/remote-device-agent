import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Startup } from '../.build/startup.js';
import { secureNewPath } from '../.build/platform.js';
function fixture(t, granted = () => false) {
  const dir = fs.realpathSync.native(fs.mkdtempSync(path.join(os.tmpdir(), 'rdh-startup-')));
  secureNewPath(dir, true);
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  let enabled = false;
  let now = Date.now();
  const calls = [];
  const backend = {
    enabled: () => enabled,
    set: (binding, value) => {
      calls.push([binding.mode, value]);
      enabled = value;
    },
    createUser: () => {
      calls.push(['create']);
      return { mode: 'user', name: 'rdh.service' };
    },
  };
  const startup = new Startup(path.join(dir, 'agent.json'), backend, granted, () => now);
  return {
    startup,
    calls,
    backend,
    expire: () => {
      now += 600_001;
    },
  };
}
test('remote enable waits for exact local approval; replay fails; disabling never stops', async (t) => {
  const { startup, calls } = fixture(t);
  const request = await startup.set('user');
  assert.equal(request.status, 'pending_local_approval');
  assert.deepEqual(calls, []);
  assert.equal((await startup.set('user')).request_id, request.request_id);
  await assert.rejects(startup.approve('different', true), { code: 'startup_request_expired' });
  assert.equal((await startup.approve(request.request_id, true)).mode, 'user');
  await assert.rejects(startup.approve(request.request_id, true), {
    code: 'startup_request_expired',
  });
  assert.equal((await startup.set('none')).mode, 'none');
  assert.deepEqual(calls, [['create'], ['user', true], ['user', false]]);
});
test('disable cancels pending requests and expiry prevents approval', async (t) => {
  const { startup, calls, expire } = fixture(t);
  const first = await startup.set('user');
  await startup.set('none');
  await assert.rejects(startup.approve(first.request_id, true), {
    code: 'startup_request_expired',
  });
  const next = await startup.set('user');
  expire();
  await assert.rejects(startup.approve(next.request_id, true), { code: 'startup_request_expired' });
  assert.deepEqual(calls, []);
});
test('grant is mode-specific and checked each time; no remote elevation or identity migration', async (t) => {
  let grant = true;
  const { startup, calls } = fixture(t, (_, mode) => grant && mode === 'user');
  assert.equal((await startup.set('user')).mode, 'user');
  grant = false;
  await startup.set('none');
  assert.equal((await startup.set('user')).status, 'pending_local_approval');
  await assert.rejects(startup.set('service', true), { code: 'startup_binding_conflict' });
  assert.deepEqual(calls, [['create'], ['user', true], ['user', false]]);
});
test('approval is not overwritten by conflicting request; denial clears it', async (t) => {
  const { startup } = fixture(t);
  const request = await startup.set('user');
  await assert.rejects(startup.set('service'), { code: 'startup_busy' });
  assert.equal((await startup.approve(request.request_id, false)).status, 'denied');
  assert.equal(startup.status().pending, null);
});
test('service needs preinstalled binding and native permission failures remain explicit', async (t) => {
  const { startup, backend } = fixture(t);
  await assert.rejects(startup.set('service', true), { code: 'startup_service_install_required' });
  await startup.bind('service', 'RemoteDeviceHub');
  backend.set = () => {
    throw new Error('access denied');
  };
  await assert.rejects(startup.set('none'), { code: 'startup_permission_required' });
});
test('parallel writers honor the lock winner and cannot approve a completed cancellation', async (t) => {
  const { startup } = fixture(t);
  const request = await startup.set('user');
  const results = await Promise.allSettled([
    startup.set('none'),
    startup.approve(request.request_id, true),
  ]);
  const [cancel, approval] = results;
  // Filesystem lock acquisition is not FIFO. A rejected cancellation never
  // took effect; if both complete, approval must have preceded cancellation.
  if (cancel.status === 'fulfilled') {
    assert.equal(startup.status().mode, approval.status === 'fulfilled' ? 'none' : 'unknown');
    assert.equal(startup.status().pending, null);
    await assert.rejects(startup.approve(request.request_id, true), {
      code: 'startup_request_expired',
    });
  } else {
    assert.equal(cancel.reason.code, 'startup_busy');
    assert.equal(approval.status, 'fulfilled');
    assert.equal(startup.status().mode, 'user');
  }
  if (approval.status === 'rejected') {
    assert.ok(['startup_busy', 'startup_request_expired'].includes(approval.reason.code));
  }
});

test('Agent exposes startup only when configured and rejects remote approval parameters', async (t) => {
  const { Agent } = await import('../.build/agent.js');
  const { Policy } = await import('../.build/policy.js');
  const { startup, calls } = fixture(t);
  const config = {
    roots: { work: path.dirname(startup.config) },
    allow_shell: false,
    allow_write: false,
    max_processes: 1,
    max_process_seconds: 10,
  };
  const agent = new Agent(config, { startup });
  t.after(() => agent.close());
  assert.ok(agent.info().capabilities.operations.includes('set_startup'));
  await assert.rejects(
    agent.dispatch('set_startup', { mode: 'user', approved: true }),
    /Unknown startup parameter/,
  );
  await assert.rejects(
    agent.dispatch('get_startup', { approval: true }),
    /Unknown startup parameter/,
  );
  await assert.rejects(agent.dispatch('set_startup', { mode: 'invalid' }), /mode must/);
  assert.deepEqual(calls, []);
  assert.equal(
    (await agent.dispatch('set_startup', { mode: 'user' })).status,
    'pending_local_approval',
  );
  const denied = new Agent(config, {
    startup,
    policy: new Policy({ allowed_operations: ['device_info'] }),
  });
  t.after(() => denied.close());
  await assert.rejects(denied.dispatch('set_startup', { mode: 'none' }), {
    code: 'policy_operation_denied',
  });
});

test('noninteractive CLI cannot supply local confirmation', async (t) => {
  const { spawnSync } = await import('node:child_process');
  const { writeConfig } = await import('../.build/config.js');
  const { startup } = fixture(t);
  await writeConfig(startup.config, {
    gateway_url: 'http://127.0.0.1:8765',
    device_id: 'startup-cli-test',
    token: 't'.repeat(40),
    roots: { work: path.dirname(startup.config) },
    allow_shell: false,
    allow_write: false,
  });
  const request = await startup.set('user');
  for (const args of [
    ['set', 'user'],
    ['approve', request.request_id],
  ]) {
    const result = spawnSync(
      process.execPath,
      ['.build/cli.js', 'startup', ...args, '--config', startup.config],
      { encoding: 'utf8', input: 'yes\n' },
    );
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /interactive terminal/);
  }
  assert.equal(startup.status().pending.request_id, request.request_id);
});

test('unprivileged service identity reports administrator requirement before soliciting approval', async (t) => {
  const { startup, backend, calls } = fixture(t);
  await startup.bind('service', 'RemoteDeviceHub');
  backend.serviceAuthority = () => false;
  await assert.rejects(startup.set('service'), { code: 'startup_permission_required' });
  assert.equal(startup.status().pending, null);
  assert.deepEqual(calls, []);
});

test('launchctl state accepts current words and older booleans without confusing labels', async () => {
  const { launchdEnabled } = await import('../.build/startup-native.js');
  for (const value of ['true', 'disabled']) {
    assert.equal(
      launchdEnabled(
        `disabled services = { "com.be-one-click.rdh" => ${value} }`,
        'com.be-one-click.rdh',
      ),
      false,
    );
  }
  for (const value of ['false', 'enabled']) {
    assert.equal(
      launchdEnabled(
        `disabled services = { "com.be-one-click.rdh" => ${value} }`,
        'com.be-one-click.rdh',
      ),
      true,
    );
  }
  assert.equal(
    launchdEnabled('disabled services = { "com.other" => disabled }', 'com.be-one-click.rdh'),
    true,
  );
  assert.throws(
    () =>
      launchdEnabled(
        'disabled services = { "com.be-one-click.rdh" => invalid }',
        'com.be-one-click.rdh',
      ),
    /Unrecognized/,
  );
});
