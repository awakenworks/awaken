// Capture-decision projection (ADR-0050 D2/D8): the effective content-capture
// level is the `meet` of the (env) ceiling × the requested level × the subject's
// consent, with a reason code. This drives the full invariant over the real
// binary with the env ceiling raised to `full`, so CONSENT is the binding gate.
//
// Run: (from e2e/)  node management_capture_decision_e2e.mjs

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

async function main() {
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
    { AWAKEN_CONTENT_CAPTURE: 'full' },
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
