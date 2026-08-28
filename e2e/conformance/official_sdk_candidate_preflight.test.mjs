import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const repo = path.resolve(here, '../..');

test('candidate preflight rejects same-version source mutation before importing SDK code', () => {
  // Causal attack graph: A1 an attacker preserves the trusted package version;
  // A2 they mutate an allowlisted shared-runtime path; A3 the changed module has
  // an observable top-level effect. Security effects: S1 qualification rejects
  // the byte-different package; S2 A3 never occurs. This process boundary proves
  // source extraction and qualification precede every candidate dynamic import,
  // rather than merely proving that the pure hash comparator detects a mismatch.
  const root = fs.mkdtempSync(path.join(tmpdir(), 'awaken-sdk-preflight-'));
  const candidateRoot = path.join(root, 'sdk');
  const marker = path.join(root, 'candidate-executed');
  try {
    fs.cpSync(resolveSdkPackage('@anthropic-ai/sdk-current').root, candidateRoot, {
      recursive: true,
    });
    const middleware = path.join(candidateRoot, 'core/middleware.mjs');
    const original = fs.readFileSync(middleware, 'utf8');
    fs.writeFileSync(
      middleware,
      `import { writeFileSync } from 'node:fs';\nwriteFileSync(${JSON.stringify(marker)}, 'executed');\n${original}`,
    );

    const result = spawnSync(
      process.execPath,
      [path.join(here, 'sdk_latest_runtime_canary.mjs')],
      {
        cwd: repo,
        encoding: 'utf8',
        env: { ...process.env, ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: candidateRoot },
      },
    );
    assert.notEqual(result.status, 0, 'S1 tampered candidate is rejected');
    assert.match(
      `${result.stdout}\n${result.stderr}`,
      /same-version SDK package content differs/u,
      'S1 rejection is the qualification boundary',
    );
    assert.equal(fs.existsSync(marker), false, 'S2 candidate module was never evaluated');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
