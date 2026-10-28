import os from 'node:os';
import { platformName } from '../src/platform.js';

console.log(
  JSON.stringify({
    os: platformName(),
    hostname: os.hostname(),
    release: os.release(),
  }),
);
