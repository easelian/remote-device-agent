#!/usr/bin/env node
import { launch } from './bootstrap.js';

const argv = process.argv.slice(2);
const command = argv[0];

if (command === 'upgrade' || command === 'rollback') {
  const { runUpgrade } = await import('./upgrade.js');
  runUpgrade(command, argv.slice(1)).catch((error: any) => {
    console.error(`Remote Device Hub：${error.message}`);
    process.exitCode = 1;
  });
} else {
  launch(argv).catch((error) => {
    console.error(`Remote Device Hub：${error.message}`);
    process.exitCode = 1;
  });
}
