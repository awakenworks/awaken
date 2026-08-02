// Capture-decision projection (ADR-0050 D2/D8): the effective content-capture
// level is the `meet` of the deployment ceiling × the requested level × the subject's
// consent, with a reason code. This drives the full invariant over the real
// binary with the typed ceiling raised to `full`, so CONSENT is the binding gate.
//
// Cause graph / decision table:
//   C1 typed deployment ceiling=full; C2 requested level; C3 subject consent.
//   The effective level is `meet(C1,C2,C3)` and the lowest binding cause owns
//   the reason. Environment variables are deliberately not a configuration path.
//
// | Rule | ceiling | requested | consent | effective | reason |
// |---|---|---|---|---|---|
// | D1 | full | full | structured | structured | no_consent |
// | D2 | full | full | full | full | ok |
// | D3 | full | structured | full | structured | ok |
//
// Run: (from e2e/)  node management_capture_decision_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { deploymentEnv, withScenarioServer, pass } from './harness.mjs';

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-capture-decision-'));
  const env = deploymentEnv(directory, {
    identityMode: 'no-login',
    fields: { content_capture: 'full' },
  });
  try {
    await withScenarioServer(
      'management',
      'mcp',
      38193,
      async (baseUrl) => {
      const id = 'dsub_cd_e2e';

      // Ceiling is `full` (env), but no consent yet → consent caps at structured,
      // so a `full` request lands `structured` with reason `no_consent`.
      let res = await fetch(
        `${baseUrl}/v1/user_profiles/${id}/capture-decision?requested=full`,
      );
      assert.equal(res.status, 200, `status ${res.status}`);
      let body = await res.json();
      assert.equal(body.ceiling, 'full', 'env ceiling is full');
      assert.equal(body.consent, 'structured', 'no consent → structured cap');
      assert.equal(body.effective, 'structured', 'consent is the binding gate');
      assert.equal(body.reason, 'no_consent');
      pass('no consent → effective=structured, reason=no_consent');

      // Grant telemetry_content consent.
      const g = await fetch(`${baseUrl}/v1/user_profiles/${id}/consent`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ purpose: 'telemetry_content', version: 'v1' }),
      });
      assert.equal(g.status, 200);

      // Now ceiling × requested × consent all permit full → effective full, ok.
      res = await fetch(
        `${baseUrl}/v1/user_profiles/${id}/capture-decision?requested=full`,
      );
      body = await res.json();
      assert.equal(body.consent, 'full', 'consent now permits full');
      assert.equal(body.effective, 'full', 'all three permit full');
      assert.equal(body.reason, 'ok');
      pass('after consent → effective=full, reason=ok');

      // A caller requesting only `structured` gets exactly that (ok), never more.
      res = await fetch(
        `${baseUrl}/v1/user_profiles/${id}/capture-decision?requested=structured`,
      );
      body = await res.json();
      assert.equal(body.effective, 'structured');
      assert.equal(body.reason, 'ok');
      pass('requested=structured → effective=structured, reason=ok');
      },
      env,
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
