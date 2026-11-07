'use strict';

const { appendFileSync } = require('node:fs');
const { execFileSync } = require('node:child_process');
const { setTimeout: sleep } = require('node:timers/promises');

// Only a release of this exact tag and commit can own the eight native checks.
// API failures, incomplete scheduling and unsuccessful releases retain normal CI.
async function findRelease({ repository, sha, tag, request }) {
  const base = `/repos/${repository}/actions`;
  const { workflow_runs: runs } = await request(
    `${base}/workflows/release.yml/runs?head_sha=${sha}&per_page=100`,
  );
  for (const run of runs) {
    if (run.head_sha !== sha || run.head_branch !== tag
      || run.path !== '.github/workflows/release.yml'
      || !['push', 'workflow_dispatch'].includes(run.event)
      || !['queued', 'in_progress', 'completed'].includes(run.status)
      || (run.status === 'completed' && run.conclusion !== 'success')) continue;
    const { jobs } = await request(`${base}/runs/${run.id}/jobs?filter=latest&per_page=100`);
    const native = jobs.filter(job => job.name.startsWith('native / Development checks ('));
    if (jobs.some(job => job.name === 'preflight' && job.conclusion === 'success')
      && native.length === 8 && new Set(native.map(job => job.name)).size === 8
      && native.every(job => ['queued', 'in_progress'].includes(job.status)
        || (job.status === 'completed' && job.conclusion === 'success'))) return run.id;
  }
  return null;
}

async function plan({ event, ref, taggedSha, sha, tag, repository, request, wait = sleep }) {
  if (event !== 'push' || ref !== 'refs/heads/main' || taggedSha !== sha) {
    return { nativeRequired: true };
  }
  // Atomic branch+tag pushes can expose the branch run before the release jobs.
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      const release = await findRelease({ repository, sha, tag, request });
      if (release) return { nativeRequired: false, release };
    } catch {
      return { nativeRequired: true };
    }
    if (attempt < 3) await wait(10000);
  }
  return { nativeRequired: true };
}

async function main() {
  const tag = process.argv[2];
  if (!/^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/.test(tag)) {
    throw new Error('Expected the package version tag');
  }
  let taggedSha;
  try {
    taggedSha = execFileSync('git', ['rev-parse', '--verify', `refs/tags/${tag}^{commit}`],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
  } catch { /* An ordinary commit has no matching release tag. */ }
  const result = await plan({
    event: process.env.GITHUB_EVENT_NAME, ref: process.env.GITHUB_REF,
    sha: process.env.GITHUB_SHA, repository: process.env.GITHUB_REPOSITORY, tag, taggedSha,
    request: async path => {
      const response = await fetch(`${process.env.GITHUB_API_URL || 'https://api.github.com'}${path}`, {
        headers: { Authorization: `Bearer ${process.env.GH_TOKEN}`, Accept: 'application/vnd.github+json' },
        signal: AbortSignal.timeout(5000),
      });
      if (!response.ok) throw new Error('Release status unavailable');
      return response.json();
    },
  });
  appendFileSync(process.env.GITHUB_OUTPUT, `native_required=${result.nativeRequired}\n`);
  const message = result.nativeRequired ? 'Run all eight native checks.'
    : `Native checks are owned by release run https://github.com/${process.env.GITHUB_REPOSITORY}/actions/runs/${result.release}.`;
  console.log(message);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${message}\n`);
}

module.exports = { plan };
if (require.main === module) main().catch(error => { console.error(error); process.exitCode = 1; });
