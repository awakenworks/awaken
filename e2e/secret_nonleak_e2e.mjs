// Secret-non-leak security invariant e2e.
//
// NUMBERING CORRECTION: this test was requested under the label "G3 secret 不泄漏",
// but in THIS repo G3 is NOT about secrets — `docs/INVARIANTS.md` G3 is
// "config-to-runtime input is serializable data (ResolvedSpec + CatalogFingerprint)".
// The ACTUAL secret-non-leak invariants here are:
//   * G8  — "secrets are opaque refs" (config carries no resolved secret material;
//           "no-secret serialization tests").
//   * G34 — a `ConnectionPlan` carries a `CredentialRef` ONLY, never resolved secret
//           material ("no-secret-serialization test: plan serializes a ref, no
//           `authorization`/`bearer` material").
// The redaction primitive backing both is `RedactedString`
// (`crates/contract/awaken-agent-contract/src/secret.rs`): redacted Debug/Display,
// deliberately NOT Serialize/Deserialize, zeroize-on-drop, `expose_secret()` the
// single trust boundary, and `preview()` yielding a masked `sk-a***wxyz`.
//
// What this proves end-to-end: a credential secret planted through the management /
// vault plane (write-only `secret_value`) NEVER surfaces as full plaintext on ANY
// read-back surface — the SDK-parsed views, the raw HTTP response bodies, the
// server's own stdout/stderr logs, or the OTel trace file. The only acceptable
// appearance of a secret is a masked `preview()` form, never the full value.
//
// Run: (from e2e/)  node secret_nonleak_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38523;
const TRACE_FILE = `/tmp/awaken-secret-nonleak-trace-${process.pid}.jsonl`;

// A unique, unguessable sentinel: if this exact string appears on any read-back
// surface, a real secret leaked. Long enough (> 12 chars) that `preview()` masks
// its middle, so a masked form can never accidentally reproduce the full value.
const SENTINEL = `sk-nonleak-SENTINEL-9f31c0dead${Date.now()}`; // awaken-allow: secret

// Assert a surface's serialized form does not contain the full sentinel. A masked
// preview (head4 + "***" + tail4) is fine; the full plaintext is a leak.
function assertNoSentinel(surface, text) {
  assert.ok(
    !text.includes(SENTINEL),
    `LEAK: sentinel secret surfaced in ${surface}:\n${text.slice(0, 2000)}`,
  );
  pass(`no secret leak on: ${surface}`);
}

async function main() {
  fs.rmSync(TRACE_FILE, { force: true });
  try {
    await withScenarioServer(
      'management',
      'mcp',
      PORT,
      async (baseUrl, _upstream, capture) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

        // 1) Plant the secret through the vault plane. `secret_value` is write-only.
        const vault = await client.beta.vaults.create({
          display_name: 'secret-nonleak vault',
          betas: BETAS,
        });
        assert.ok(vault.id.startsWith('vlt_'), `vault id: ${vault.id}`);

        const cred = await client.beta.vaults.credentials.create(vault.id, {
          type: 'environment_variable',
          secret_name: 'ANTHROPIC_API_KEY',
          secret_value: SENTINEL,
          networking: { type: 'unrestricted' },
          betas: BETAS,
        });
        assert.equal(cred.type, 'vault_credential');
        pass('planted a write-only secret via beta.vaults.credentials.create');

        // 2a) The create response itself must not echo the secret.
        assertNoSentinel('credentials.create response (SDK-parsed)', JSON.stringify(cred));

        // 2b) SDK-parsed retrieve view.
        const got = await client.beta.vaults.credentials.retrieve(cred.id, {
          vault_id: vault.id,
          betas: BETAS,
        });
        assert.equal(got.id, cred.id);
        assertNoSentinel('credentials.retrieve view (SDK-parsed)', JSON.stringify(got));

        // 2c) SDK-parsed list view (drain the paginated iterator).
        const listed = [];
        for await (const c of client.beta.vaults.credentials.list(vault.id, { betas: BETAS })) {
          listed.push(c);
        }
        assert.ok(
          listed.some((c) => c.id === cred.id),
          'created credential appears in the list view',
        );
        assertNoSentinel('credentials.list view (SDK-parsed)', JSON.stringify(listed));

        // 3) Raw HTTP response bodies (bypass the SDK parser, in case the SDK were
        //    to strip a field the server actually emits on the wire).
        const hdr = { 'anthropic-beta': BETAS[0], authorization: 'Bearer e2e-dummy' };
        const rawGet = await fetch(
          `${baseUrl}/v1/vaults/${vault.id}/credentials/${cred.id}?beta=true`,
          { headers: hdr },
        );
        assert.equal(rawGet.status, 200, 'raw credential GET ok');
        assertNoSentinel('raw HTTP GET credential body', await rawGet.text());

        const rawList = await fetch(
          `${baseUrl}/v1/vaults/${vault.id}/credentials?beta=true`,
          { headers: hdr },
        );
        assert.equal(rawList.status, 200, 'raw credential LIST ok');
        assertNoSentinel('raw HTTP LIST credentials body', await rawList.text());

        // The vault retrieve view must not carry credential material either.
        const rawVault = await fetch(`${baseUrl}/v1/vaults/${vault.id}?beta=true`, {
          headers: hdr,
        });
        assert.equal(rawVault.status, 200, 'raw vault GET ok');
        assertNoSentinel('raw HTTP GET vault body', await rawVault.text());

        // Give the server a moment to flush any deferred log lines from the writes
        // above before we scan its captured stdio.
        await new Promise((r) => setTimeout(r, 250));

        // 4) The server's own stdout/stderr logs (Debug/Display of any resolved
        //    credential must be `RedactedString(***)` / `***`, never plaintext).
        assert.ok(capture, 'server stdio capture is available');
        assertNoSentinel('captured server stdout+stderr', capture.text());
      },
      { AWAKEN_TRACE_FILE: TRACE_FILE },
      { capture: true },
    );

    // 5) The OTel trace file (span attributes/events), if the server wrote one.
    if (fs.existsSync(TRACE_FILE)) {
      assertNoSentinel('OTel trace file', fs.readFileSync(TRACE_FILE, 'utf8'));
    } else {
      pass('no trace file emitted (nothing to scan)');
    }

    pass('secret non-leak (G8/G34)');
    console.log(
      'E2E PASS: the planted vault secret never surfaced on any read-back, log, or trace surface.',
    );
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    fs.rmSync(TRACE_FILE, { force: true });
  }
}

main();
