#!/usr/bin/env node
import { launch } from './bootstrap.js';

launch(process.argv.slice(2)).catch((error) => {
  console.error(`Remote Device Hub：${error.message}`);
  process.exitCode = 1;
});
