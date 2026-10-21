#!/usr/bin/env node
import { build } from 'esbuild';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import path from 'node:path';

const root = fileURLToPath(new URL('../', import.meta.url));
const sources = (await readdir(path.join(root, 'src'))).filter(
  (name) => name.endsWith('.ts') && !name.endsWith('.d.ts'),
);
// Readable modules are for tests and development only. They never enter npm.
await build({
  absWorkingDir: root,
  entryPoints: sources.map((name) => path.join(root, 'src', name)),
  outdir: path.join(root, '.build'),
  platform: 'node',
  format: 'esm',
  target: 'node22',
  sourcemap: false,
  logLevel: 'warning',
});
await mkdir(path.join(root, 'bundle'), { recursive: true });
// Keep Node built-ins external. Inline only the audited, vendored ws client.
// createRequire supports its CommonJS references to built-in Node modules.
await build({
  absWorkingDir: root,
  entryPoints: [path.join(root, 'src/cli.ts')],
  outfile: path.join(root, 'bundle/agent.mjs'),
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node22',
  minify: true,
  keepNames: true,
  sourcemap: false,
  legalComments: 'none',
  banner: {
    js: 'import { createRequire as __rdhCreateRequire } from "node:module"; const require = __rdhCreateRequire(import.meta.url);',
  },
  define: { 'process.env.WS_NO_BUFFER_UTIL': '"1"', 'process.env.WS_NO_UTF_8_VALIDATE': '"1"' },
  metafile: true,
  logLevel: 'warning',
}).then(async (result) => {
  // Only built-in Node imports may remain at runtime; no native accelerators,
  // registry packages, TypeScript sources or source maps may be fetched later.
  for (const output of Object.values(result.metafile.outputs)) {
    for (const entry of output.imports) {
      if (
        !entry.external ||
        (!entry.path.startsWith('node:') &&
          ![
            'events',
            'https',
            'http',
            'net',
            'tls',
            'crypto',
            'stream',
            'url',
            'zlib',
            'buffer',
            'util',
          ].includes(entry.path))
      ) {
        throw new Error(`Unexpected runtime import: ${entry.path}`);
      }
    }
  }
  await writeFile(
    path.join(root, '.build/bundle-meta.json'),
    JSON.stringify(result.metafile, null, 2) + '\n',
  );
});
// Licenses/provenance accompany the compiled client, not the full vendor tree.
await writeFile(
  path.join(root, 'THIRD_PARTY_LICENSES'),
  await readFile(path.join(root, 'vendor/ws/LICENSE')),
);
const { compilationFingerprint } = await import('./release.mjs');
await writeFile(
  path.join(root, '.build/compilation.json'),
  JSON.stringify({
    inputs: compilationFingerprint(),
    bundle: createHash('sha256')
      .update(await readFile(path.join(root, 'bundle/agent.mjs')))
      .digest('hex'),
  }) + '\n',
);
console.log('Built TypeScript development modules and compact Agent CLI.');
