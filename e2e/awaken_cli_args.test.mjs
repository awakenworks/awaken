import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { automatedAllInOneArgs } from './awaken_cli_args.mjs';

const E2E_ROOT = path.dirname(fileURLToPath(import.meta.url));

function sourceFiles(directory) {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const target = path.join(directory, entry.name);
    if (entry.isDirectory()) return sourceFiles(target);
    return entry.isFile() && /\.(?:mjs|js|ts)$/u.test(entry.name) ? [target] : [];
  });
}

test('automated all-in-one invocations are headless by construction', () => {
  // Cause/effect graph: C1 an automated scenario starts all-in-one; C2 it may
  // supply ordered service options. E1 the canonical role remains first; E2
  // exactly one --no-browser override is present; E3 caller options retain order.
  //
  // | Rule | C1 | C2 | role | no-browser count | option result |
  // |---|---|---|---|---|---|
  // | H1 | yes | none | all-in-one | 1 | none |
  // | H2 | yes | config+port | all-in-one | 1 | unchanged order |
  const bare = automatedAllInOneArgs();
  assert.deepEqual(bare, ['all-in-one', '--no-browser'], 'H1');

  const configured = automatedAllInOneArgs('--config', '/tmp/config.toml', '--port', '38080');
  assert.deepEqual(
    configured,
    ['all-in-one', '--no-browser', '--config', '/tmp/config.toml', '--port', '38080'],
    'H2',
  );
  assert.equal(configured.filter((value) => value === '--no-browser').length, 1, 'H1/H2');
});

test('e2e sources cannot recreate raw all-in-one argument arrays', () => {
  // Duplication guard cause/effect rule: C1 an E2E source other than this owner
  // embeds a raw array beginning with all-in-one. E1 fail the suite so it must
  // consume automatedAllInOneArgs and therefore cannot accidentally open a
  // desktop browser. Files that do not launch Awaken are unaffected.
  const ownerFiles = new Set([
    path.join(E2E_ROOT, 'awaken_cli_args.mjs'),
    path.join(E2E_ROOT, 'awaken_cli_args.test.mjs'),
  ]);
  const offenders = sourceFiles(E2E_ROOT)
    .filter((file) => !ownerFiles.has(file))
    .filter((file) => /\[\s*['"]all-in-one['"]/u.test(fs.readFileSync(file, 'utf8')))
    .map((file) => path.relative(E2E_ROOT, file));

  assert.deepEqual(offenders, []);
});
