#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const { dirname, join, resolve } = require('node:path');

const arch = { x64: 'x64', arm64: 'arm64' }[process.arch];
let platform;
if (arch && process.platform === 'linux') {
  const report = process.report && process.report.getReport();
  if (!report || !report.header) {
    console.error('Cannot determine the Linux C library for this Node.js runtime.');
    process.exit(1);
  }
  platform = `linux-${arch}-${report.header.glibcVersionRuntime ? 'gnu' : 'musl'}`;
} else if (arch && (process.platform === 'darwin' || process.platform === 'win32')) {
  platform = `${process.platform}-${arch}`;
}
if (!platform) {
  console.error(`Unsupported platform: ${process.platform}/${process.arch}`);
  process.exit(1);
}
const nativePackage = `@easelian/remote-device-agent-${platform}`;
let nativeManifest;
try {
  nativeManifest = require.resolve(`${nativePackage}/package.json`);
} catch (error) {
  if (error.code !== 'MODULE_NOT_FOUND') throw error;
  const version = require('./package.json').version;
  console.error(`Missing the Agent binary for ${platform}. Reinstall with optional dependencies enabled: npm install -g @easelian/remote-device-agent@${version} --include=optional`);
  process.exit(1);
}
const executable = join(dirname(nativeManifest), process.platform === 'win32' ? 'rdh.exe' : 'rdh');
const args = process.argv.slice(2);
const options = { stdio: 'inherit', windowsHide: false };
if (args.length === 1 && ['doctor', 'update'].includes(args[0])) {
  options.env = { ...process.env, RDH_NPM_ENTRY: resolve(process.argv[1]) };
}
const child = spawn(executable, args, options);
child.on('error', (error) => {
  console.error(`Unable to start the native Agent: ${error.message}`);
  process.exitCode = 1;
});
const signals = ['SIGINT', 'SIGTERM', 'SIGHUP'];
if (process.platform === 'win32') signals.push('SIGBREAK');
for (const signal of signals) {
  try {
    process.on(signal, () => {
      // Console Ctrl+C/Break reaches the shared Windows process group. Node's
      // child.kill would forcibly terminate the Agent before graceful cleanup.
      if (process.platform === 'win32' && (signal === 'SIGINT' || signal === 'SIGBREAK')) return;
      if (child.exitCode === null && child.signalCode === null) child.kill(signal);
    });
  } catch (_) {
    // Some signals are not available on Windows.
  }
}
child.on('exit', (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    try { process.kill(process.pid, signal); } catch (_) { process.exitCode = 1; }
  } else {
    process.exitCode = code === null ? 1 : code;
  }
});
