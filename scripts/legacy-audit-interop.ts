// Test-only writer for the legacy Agent's actual local audit format.
import { AuditLog } from '../src/audit.js';

const [directory, label, countText, maxBytesText, requiredText] = process.argv.slice(2);
const count = Number(countText);
const maxBytes = Number(maxBytesText);
if (!directory || !label || !Number.isInteger(count) || count < 0 || !Number.isInteger(maxBytes)) {
  throw new Error('The fixture requires a directory, label, count and byte limit');
}
const log = new AuditLog({
  directory,
  name: 'fixture',
  maxBytes,
  maxFiles: 3,
  required: requiredText === 'true',
});
try {
  for (let index = 0; index < count; index++) {
    log.record({
      id: `${label}-${index}`,
      operation: 'write_file',
      status: 'succeeded',
      durationMs: 7,
      params: { root: 'work', path: 'fixture.txt', content: 'AUDIT_FIXTURE_SECRET' },
      result: { size_bytes: 20, output: 'AUDIT_FIXTURE_SECRET' },
    });
  }
  console.log(JSON.stringify(log.stats()));
} finally {
  log.close();
}
