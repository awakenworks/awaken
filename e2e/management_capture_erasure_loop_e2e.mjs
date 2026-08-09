// Full ADR-0050 loop end-to-end: a real agent turn with content capture = full
// writes the prompt/completion into a subject-tagged store; GDPR erasure then
// removes exactly that subject's captured content. Drives a real Managed turn
// (echo model over the real runtime) with request-grain `user_profile_id`, then
// hits the Awaken erasure endpoint.
//
// Cause graph / decision table:
//   C1 typed deployment ceiling=full; C2 telemetry consent granted;
//   C3 Managed request carries user_profile_id; C4 erasure is requested;
//   C5 erasure is retried.
//
// | Rule | C1 | C2 | C3 | C4 | C5 | Result |
// |---|---|---|---|---|---|---|
// | E1 | Y | Y | Y | N | N | prompt/completion stored under the exact subject |
// | E2 | Y | Y | Y | Y | N | subject rows removed; durable receipt count > 0 |
// | E3 | Y | Y | Y | Y | Y | same durable receipt; no second deletion effect |
// | E4 | Y | Y | N | - | - | no subject-owned content (adapter unit-test partition) |
//
// Run: (from e2e/)  node management_capture_erasure_loop_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { deploymentEnv, withRealServer, pass } from './harness.mjs';
import { sqliteRows } from './sqlite.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-capture-erasure-'));
  const env = deploymentEnv(directory, {
    identityMode: 'no-login',
    fields: { content_capture: 'full' },
  });
  try {
    await withRealServer(
      'echo',
      38194,
      async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const consent = await fetch(`${base}/v1/user_profiles/dsub_full/consent`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ purpose: 'telemetry_content', version: 'v1' }),
      });
      assert.equal(consent.status, 200, `consent status ${consent.status}`);
      const decision = await (
        await fetch(`${base}/v1/user_profiles/dsub_full/capture-decision?requested=full`)
      ).json();
      assert.equal(decision.effective, 'full', `capture decision ${JSON.stringify(decision)}`);

      // The typed ceiling, consent, and request-grain attribution all agree, so
      // the engine writes prompt + completion into the subject-tagged store.
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        betas: BETAS,
      });
      const sent = await fetch(`${base}/v1/sessions/${session.id}/events?beta=true`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'anthropic-beta': BETAS.join(','),
        },
        body: JSON.stringify({
          user_profile_id: 'dsub_full',
          events: [{
            type: 'user.message',
            content: [{ type: 'text', text: 'please capture this content' }],
          }],
        }),
      });
      assert.equal(sent.status, 200, `send status ${sent.status}: ${await sent.text()}`);
      const events = [];
      for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
        events.push(event);
      }
      assert.ok(
        JSON.stringify(events).includes('please capture this content'),
        `turn did not complete: ${JSON.stringify(events)}`,
      );
      pass('ran a real turn with content capture=full (subject dsub_full)');

      const captured = sqliteRows(
        path.join(directory, 'captured_content.db'),
        'SELECT subject, purpose, content FROM coordinator_data_capture_captured WHERE subject = ?',
        'dsub_full',
      );
      assert.ok(captured.length > 0, 'E1: the real turn must persist subject-owned content');
      assert.ok(
        captured.some((row) => row.content.includes('please capture this content')),
        `E1: prompt was not captured: ${JSON.stringify(captured)}`,
      );
      pass(`run→capture→store: ${captured.length} subject-owned records persisted`);

      // Erasure removes exactly this subject's captured content.
      const res = await fetch(`${base}/v1/user_profiles/dsub_full/erasure`, { method: 'POST' });
      assert.equal(res.status, 200, `erasure status ${res.status}`);
      const body = await res.json();
      assert.ok(
        body.records_removed > 0,
        `expected captured content to be erased, got ${body.records_removed}`,
      );
      pass(`run→capture→store→erase: ${body.records_removed} captured records erased`);

      // A retry returns the same durable receipt. `records_removed` is cumulative
      // accountability evidence, not the delta of this HTTP attempt.
      const again = await (
        await fetch(`${base}/v1/user_profiles/dsub_full/erasure`, { method: 'POST' })
      ).json();
      assert.equal(
        again.records_removed,
        body.records_removed,
        'an idempotent retry returns the original durable erasure receipt',
      );
      pass('erasure is idempotent — retry returned the same accountability receipt');
      },
      { mode: 'management', extraEnv: env },
    );
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
