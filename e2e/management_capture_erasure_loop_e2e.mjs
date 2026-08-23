// Managed Agents / ADR-0050 compatibility boundary: Session events use the
// official closed envelope and therefore cannot carry the removed
// `user_profile_id` extension. Prove the rejected request has no runtime or
// capture side effect, then prove the official event remains usable and an
// erasure retry returns a stable zero-effect receipt.
//
// Cause graph / decision table:
//   C1 typed deployment ceiling=full; C2 telemetry consent granted;
//   C3 strict Managed JSON carries removed user_profile_id; C4 erasure is
//   requested; C5 erasure is retried.
//
// | Rule | C1 | C2 | C3 | C4 | C5 | Result |
// |---|---|---|---|---|---|---|
// | E1 | Y | Y | Y | N | N | 400; no append, Run, or subject-owned capture |
// | E2 | Y | Y | N | N | N | official event is accepted without attribution |
// | E3 | Y | Y | Y | Y | N | zero-effect durable erasure receipt |
// | E4 | Y | Y | Y | Y | Y | same durable receipt; no second deletion effect |
//
// Constraint K: `anthropic-user-profile-id` is the canonical request-context
// carrier and is therefore not an invalid envelope extension. E1 places the
// forbidden field in the strict JSON root; E2 omits both field and header.
//
// Run: (from e2e/)  node management_capture_erasure_loop_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  USER_PROFILES_BETA,
  deploymentEnv,
  withRealServer,
  waitForSessionEventReceipt,
  pass,
} from './harness.mjs';
import { sqliteRows } from './sqlite.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PROFILE_HEADERS = { 'anthropic-beta': USER_PROFILES_BETA };

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
        headers: { ...PROFILE_HEADERS, 'content-type': 'application/json' },
        body: JSON.stringify({ purpose: 'telemetry_content', version: 'v1' }),
      });
      assert.equal(consent.status, 200, `consent status ${consent.status}`);
      const decision = await (
        await fetch(`${base}/v1/user_profiles/dsub_full/capture-decision?requested=full`, {
          headers: PROFILE_HEADERS,
        })
      ).json();
      assert.equal(decision.effective, 'full', `capture decision ${JSON.stringify(decision)}`);

      // `user_profile_id` was removed from the official Session event envelope.
      // Keeping this as a negative test prevents an attractive but incompatible
      // extension from being reintroduced.
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const rejected = await fetch(`${base}/v1/sessions/${session.id}/events?beta=true`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'anthropic-beta': BETAS.join(','),
        },
        body: JSON.stringify({
          events: [{
            type: 'user.message',
            content: [{ type: 'text', text: 'please capture this content' }],
          }],
          user_profile_id: 'dsub_full',
        }),
      });
      assert.equal(rejected.status, 400, `removed extension status ${rejected.status}`);

      const before = [];
      for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
        before.push(event);
      }
      assert.equal(before.length, 0, 'rejected envelope must not append an event');

      const receipt = await client.beta.sessions.events.send(session.id, {
        betas: BETAS,
        events: [{
          type: 'user.message',
          content: [{ type: 'text', text: 'official unattributed content' }],
        }],
      });
      const { events } = await waitForSessionEventReceipt(
        client,
        session.id,
        receipt.data[0]?.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message'),
        'E2 official unattributed receipt reaches its committed reply',
      );
      assert.ok(
        JSON.stringify(events).includes('official unattributed content'),
        `turn did not complete: ${JSON.stringify(events)}`,
      );
      pass('removed user_profile_id is atomic 400; official Session event still completes');

      const captured = sqliteRows(
        path.join(directory, 'captured_content.db'),
        'SELECT subject, purpose, content FROM coordinator_data_capture_captured WHERE subject = ?',
        'dsub_full',
      );
      assert.equal(captured.length, 0, 'official unattributed event must not create subject-owned content');
      pass('rejected and official-unattributed events create no subject-owned capture');

      // Erasure removes exactly this subject's captured content.
      const res = await fetch(`${base}/v1/user_profiles/dsub_full/erasure`, {
        method: 'POST',
        headers: PROFILE_HEADERS,
      });
      assert.equal(res.status, 200, `erasure status ${res.status}`);
      const body = await res.json();
      assert.equal(body.records_removed, 0, 'no attributed content means a zero-effect erasure');
      pass('erasure returns a durable zero-effect receipt');

      // A retry returns the same durable receipt. `records_removed` is cumulative
      // accountability evidence, not the delta of this HTTP attempt.
      const again = await (
        await fetch(`${base}/v1/user_profiles/dsub_full/erasure`, {
          method: 'POST',
          headers: PROFILE_HEADERS,
        })
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
