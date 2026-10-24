import { readdirSync, readFileSync } from 'node:fs';
import { PUBLIC_URL, MCP_PUBLIC_URL } from '../.build/deploymentDefaults.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import { PUBLIC_SERVER, MCP_SERVER, sameHub, profileServers } from '../.build/server-selection.js';

test('only exact shipped deployment origins share trust and profile locks', () => {
  assert(sameHub(PUBLIC_SERVER, MCP_SERVER));
  assert.deepEqual(profileServers(MCP_SERVER), [...new Set([PUBLIC_SERVER, MCP_SERVER])]);
  for (const value of [
    'https://other.test',
    `${PUBLIC_SERVER}.evil.test`,
    PUBLIC_SERVER.replace('https:', 'http:'),
    `${PUBLIC_SERVER}:8443`,
  ]) {
    assert.equal(sameHub(PUBLIC_SERVER, value), false);
    assert.equal(sameHub(MCP_SERVER, value), false);
    assert.deepEqual(profileServers(value), [value]);
  }
});

test('runtime sources keep deployment hosts in generated defaults only', () => {
  const source = new URL('../src/', import.meta.url);
  const forbidden = [PUBLIC_URL, MCP_PUBLIC_URL].map((url) => new URL(url).hostname);
  forbidden.push('xdistribute' + '.site');
  for (const file of readdirSync(source, { recursive: true })) {
    if (!/\.(?:ts|mts)$/.test(file) || file.replaceAll('\\', '/') === 'deploymentDefaults.ts') {
      continue;
    }
    const content = readFileSync(new URL(file, source), 'utf8');
    assert.equal(
      forbidden.some((host) => content.includes(host)),
      false,
      file,
    );
  }
});
