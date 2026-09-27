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

test('candidate preflight rejects a split declaration before importing SDK code', () => {
  // Decision rules P1 ESM-only type mutation, P2 CommonJS-only mutation,
  // P3 missing ESM declaration => reject at canonical declaration admission;
  // E1 a top-level marker in the otherwise selected SDK is NEVER evaluated.
  // The process verifies the actual pre-import composition, complementing the
  // extractor's D1-D7 table without another declaration inventory or hash.
  const root = fs.mkdtempSync(path.join(tmpdir(), 'awaken-sdk-declaration-preflight-'));
  try {
    for (const [rule, extension] of [['P1', '.d.mts'], ['P2', '.d.ts'], ['P3', null]]) {
      const candidateRoot = path.join(root, rule);
      const marker = path.join(root, `${rule}-executed`);
      fs.cpSync(resolveSdkPackage('@anthropic-ai/sdk-current').root, candidateRoot, { recursive: true });
      const entry = path.join(candidateRoot, 'index.mjs');
      fs.writeFileSync(entry, `import { writeFileSync } from 'node:fs';\nwriteFileSync(${JSON.stringify(marker)}, 'executed');\n${fs.readFileSync(entry, 'utf8')}`);
      if (extension) {
        const declaration = path.join(candidateRoot, `resources/beta/sessions/events${extension}`);
        const before = fs.readFileSync(declaration, 'utf8');
        const after = before.replace('export interface BetaManagedAgentsTextBlock {', 'export interface BetaManagedAgentsTextBlock {\n    parity_only?: true;');
        assert.notEqual(after, before, `${rule} exact mutation exists`);
        fs.writeFileSync(declaration, after);
      } else {
        fs.rmSync(path.join(candidateRoot, 'resources/beta/sessions/events.d.mts'));
      }
      const result = spawnSync(process.execPath, [path.join(here, 'sdk_latest_runtime_canary.mjs')], {
        cwd: repo, encoding: 'utf8',
        env: { ...process.env, ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: candidateRoot },
      });
      assert.notEqual(result.status, 0, rule);
      assert.match(`${result.stdout}\n${result.stderr}`, extension ? /declarations disagree/u : /missing ESM declaration/u, rule);
      assert.equal(fs.existsSync(marker), false, `${rule}/E1 candidate was not imported`);
    }
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

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
