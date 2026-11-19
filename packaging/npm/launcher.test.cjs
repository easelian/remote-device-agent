'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const { EventEmitter } = require('node:events');
const source = fs.readFileSync(path.join(__dirname, 'rdh.cjs'), 'utf8');

function launch(platform, arch, glibc, withReport = true, installed = true) {
  const events = new EventEmitter();
  const proc = Object.assign(events, {
    platform, arch, argv: ['node', 'rdh', '--version', 'a b'], exitCode: undefined,
    report: withReport ? { getReport: () => ({ header: glibc ? { glibcVersionRuntime: '2.39' } : {} }) } : undefined,
    exit: (code) => { throw new Error(`exit ${code}`); },
    kill: () => {}, pid: 123,
  });
  const child = new EventEmitter();
  child.exitCode = null;
  child.signalCode = null;
  child.kill = (signal) => { child.lastSignal = signal; };
  let launched;
  const logs = [];
  function mockRequire(name) {
    if (name === 'node:child_process') return { spawn: (file, args, options) => { launched = { file, args, options }; return child; } };
    if (name === 'node:path') return path;
    if (name === './package.json') return { version: '1.2.3' };
    throw new Error(`unexpected import ${name}`);
  }
  mockRequire.resolve = (name) => {
    if (!installed) throw Object.assign(new Error('not installed'), { code: 'MODULE_NOT_FOUND' });
    return path.join('/node_modules', name);
  };
  try {
    vm.runInNewContext(source, { require: mockRequire, process: proc, __dirname: '/package', console: { error: (message) => logs.push(message) } });
  } catch (error) {
    if (!/^exit /.test(error.message)) throw error;
  }
  return { launched, proc, child, logs };
}

test('selects each supported platform and preserves arguments', () => {
  for (const [os, cpu, glibc, suffix] of [
    ['linux', 'x64', true, 'linux-x64-gnu'], ['linux', 'arm64', true, 'linux-arm64-gnu'],
    ['linux', 'x64', false, 'linux-x64-musl'], ['linux', 'arm64', false, 'linux-arm64-musl'],
    ['darwin', 'x64', false, 'darwin-x64'], ['darwin', 'arm64', false, 'darwin-arm64'],
    ['win32', 'x64', false, 'win32-x64'], ['win32', 'arm64', false, 'win32-arm64'],
  ]) {
    const { launched } = launch(os, cpu, glibc);
    assert.equal(launched.file, path.join('/node_modules/@easelian', `remote-device-agent-${suffix}`, os === 'win32' ? 'rdh.exe' : 'rdh'));
    assert.deepEqual(Array.from(launched.args), ['--version', 'a b']);
    assert.equal(launched.options.stdio, 'inherit');
  }
});

test('rejects unsupported platforms and unavailable Linux reports', () => {
  assert.match(launch('freebsd', 'x64', false).logs[0], /Unsupported platform/);
  assert.match(launch('linux', 'x64', false, false).logs[0], /Cannot determine/);
});

test('explains how to reinstall when npm omitted the platform package', () => {
  const { launched, logs } = launch('linux', 'x64', true, true, false);
  assert.equal(launched, undefined);
  assert.match(logs[0], /linux-x64-gnu.*remote-device-agent@1\.2\.3 --include=optional/);
});

test('forwards signals and exit failures', () => {
  const { proc, child } = launch('linux', 'x64', true);
  proc.emit('SIGTERM');
  assert.equal(child.lastSignal, 'SIGTERM');
  child.emit('exit', 7, null);
  assert.equal(proc.exitCode, 7);
  const failure = launch('linux', 'x64', true);
  failure.child.emit('error', new Error('missing binary'));
  assert.equal(failure.proc.exitCode, 1);
  assert.match(failure.logs[0], /missing binary/);
});

test('does not forcibly terminate a Windows child on console interrupt', () => {
  const { proc, child } = launch('win32', 'x64', false);
  proc.emit('SIGINT');
  proc.emit('SIGBREAK');
  assert.equal(child.lastSignal, undefined);
  proc.emit('SIGTERM');
  assert.equal(child.lastSignal, 'SIGTERM');
});
