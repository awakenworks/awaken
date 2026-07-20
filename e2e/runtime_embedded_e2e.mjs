// Drive the two public embedded-runtime compositions as real child processes.
// They use deterministic model adapters, so this remains hermetic while proving
// both hand-built and configuration-compiled snapshots reach committed output.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function run(example) {
  return execFileSync(
    'cargo',
    ['run', '--quiet', '--locked', '-p', 'awaken-runtime-examples', '--example', example],
    { cwd: ROOT, encoding: 'utf8', env: process.env, maxBuffer: 8 * 1024 * 1024 },
  );
}

for (const example of ['direct_runtime', 'hello_agent']) {
  const output = run(example);
  assert.match(output, /run finished: Ended\(NaturalEnd\)/, `${example} reaches a natural end`);
  assert.match(output, /\[Assistant\]/, `${example} commits an assistant message`);
  console.log(`  ok: ${example} embedded composition committed a deterministic turn`);
}

console.log('E2E PASS: public embedded Runtime entry points execute compiled and hand-built snapshots.');
