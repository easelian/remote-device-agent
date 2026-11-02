#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const { join } = require('node:path');

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
const executable = join(__dirname, 'native', platform, process.platform === 'win32' ? 'rdh.exe' : 'rdh');
const child = spawn(executable, process.argv.slice(2), { stdio: 'inherit', windowsHide: false });
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
