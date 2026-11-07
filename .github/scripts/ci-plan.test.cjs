'use strict';

const assert = require('node:assert/strict');
const { test } = require('node:test');
const { plan } = require('./ci-plan.cjs');

const sha = 'a'.repeat(40);
const run = {
  id: 42, head_sha: sha, head_branch: 'v1.2.3-rc.1',
  path: '.github/workflows/release.yml', event: 'push', status: 'in_progress', conclusion: null,
};
const jobs = [
  { name: 'preflight', status: 'completed', conclusion: 'success' },
  ...Array.from({ length: 8 }, (_, i) => ({ name: `native / Development checks (target-${i})`, status: 'queued', conclusion: null })),
];

function options(runs = [run], resultJobs = jobs) {
  return {
    event: 'push', ref: 'refs/heads/main', taggedSha: sha, sha, tag: run.head_branch,
    repository: 'example/agent', wait: async () => {},
    request: async path => path.includes('/jobs?') ? { jobs: resultJobs } : { workflow_runs: runs },
  };
}

test('an exact release with all eight checks scheduled owns validation', async () => {
  assert.deepEqual(await plan(options()), { nativeRequired: false, release: 42 });
  assert.equal((await plan(options([{ ...run, status: 'completed', conclusion: 'success' }],
    jobs.map(job => ({ ...job, status: 'completed', conclusion: 'success' }))))).nativeRequired, false);
});

test('PRs, dispatches, release calls and ordinary commits always run without API access', async () => {
  for (const override of [{ event: 'pull_request' }, { event: 'workflow_dispatch' },
    { ref: `refs/tags/${run.head_branch}` }, { taggedSha: undefined }, { taggedSha: 'b'.repeat(40) }]) {
    let calls = 0;
    assert.deepEqual(await plan({ ...options(), ...override, request: async () => { calls++; } }), { nativeRequired: true });
    assert.equal(calls, 0);
  }
});

test('wrong commit, tag, workflow, event or failed release cannot replace CI', async () => {
  for (const override of [{ head_sha: 'b'.repeat(40) }, { head_branch: 'v1.2.2' },
    { path: '.github/workflows/other.yml' }, { event: 'pull_request' },
    { status: 'completed', conclusion: 'failure' }, { status: 'completed', conclusion: 'cancelled' }]) {
    assert.deepEqual(await plan(options([{ ...run, ...override }])), { nativeRequired: true });
  }
});

test('failed preflight, missing, duplicated, skipped or failed native jobs retain CI', async () => {
  for (const incomplete of [[], jobs.slice(1), jobs.slice(0, 8), [...jobs.slice(0, 8), jobs[1]],
    jobs.map((job, i) => i === 0 ? { ...job, conclusion: 'failure' } : job),
    jobs.map((job, i) => i === 1 ? { ...job, status: 'completed', conclusion: 'failure' } : job),
    jobs.map((job, i) => i === 1 ? { ...job, status: 'completed', conclusion: 'skipped' } : job)]) {
    assert.deepEqual(await plan(options([run], incomplete)), { nativeRequired: true });
  }
});

test('API errors and malformed responses retain all checks', async () => {
  for (const request of [async () => { throw new Error('403 or timeout'); }, async () => ({}), async () => ({ workflow_runs: [run] })]) {
    assert.deepEqual(await plan({ ...options(), request }), { nativeRequired: true });
  }
});

test('waits a bounded time for release scheduling, then retains CI', async () => {
  let requests = 0;
  const delays = [];
  const result = await plan({ ...options(), request: async () => { requests++; return { workflow_runs: [] }; }, wait: async ms => delays.push(ms) });
  assert.deepEqual(result, { nativeRequired: true });
  assert.equal(requests, 4);
  assert.deepEqual(delays, [10000, 10000, 10000]);
});

test('detects the matching release when its jobs appear after the branch push', async () => {
  let attempts = 0;
  const config = options();
  const request = async path => {
    if (!path.includes('/jobs?') && attempts++ === 0) return { workflow_runs: [] };
    return config.request(path);
  };
  assert.deepEqual(await plan({ ...config, request }), { nativeRequired: false, release: 42 });
  assert.equal(attempts, 2);
});
