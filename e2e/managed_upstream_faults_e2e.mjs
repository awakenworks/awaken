// Real Anthropic HTTP -> adapter evidence -> durable Run -> public Session.
// Run: (from e2e/) node managed_upstream_faults_e2e.mjs
import assert from 'node:assert/strict';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree, fakeAnthropicServerEnv, listManagedSessionEvents, withServer,
  pass, waitForRetainedUnknownExecution, waitForSessionEventReceipt,
} from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-faults-key'; // awaken-allow(secret):

async function main() {
  // Cause-effect decision table: C1 protocol-complete success / HTTP 503;
  // C2 complete / interrupted error body; C3 later success / always rejects;
  // C4 an accepted exact receipt and repeated public reads.
  // F1 success => one request, one exact reply, no error (positive control).
  // F2 complete 503 + later success => two requests, one reply, available Session.
  // F3 complete 503 + persistent rejection => three requests (default retry
  // budget), one stable unknown_error/exhausted, available Session, no reply.
  // F4/F5 interrupted 503 + either C3 => one request, no reply, one stable
  // unknown_error/terminal before logical Thread idle; physical slot retained.
  // Their input remains unprocessed: the canonical retained-unknown oracle
  // owns that distinction AND checks the real Dispatch slot, not elapsed time.
  // A status line and hypothetical later success cannot prove remote termination.
  // C4 proves these are committed outcomes, not an HTTP admission rejection.
  // Runtime failure.rs owns retries with positive NotDispatched evidence;
  // execution.rs covers Unknown across retry/failover/tolerance and recovery.
  // The Kani model_execution_reentry_requires_positive_evidence kernel owns
  // the exhaustive three-valued gate; this HTTP test checks its adapter wiring.
  for (const [rule, options, outcome, attempts] of [
    ['F1', {}, 'success', 1],
    ['F2', { failuresBeforeSuccess: 1 }, 'success', 2],
    ['F3', { alwaysFail: true }, 'exhausted', 3],
    ['F4', { failuresBeforeSuccess: 1, faultBodyMode: 'truncated' }, 'unknown', 1],
    ['F5', { alwaysFail: true, faultBodyMode: 'truncated' }, 'unknown', 1],
  ]) {
    const success = outcome === 'success';
    const unknown = outcome === 'unknown';
    const directory = mkdtempSync(join(tmpdir(), 'awaken-upstream-fault-'));
    const upstream = await startFakeAnthropic(FAKE_KEY, options);
    try {
      await withServer('real', Number(process.env.E2E_PORT), async (baseUrl) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, maxRetries: 0 });
        const session = await client.beta.sessions.create({
          agent: 'assistant', environment_id: 'env_local', betas: BETAS,
        });
        const text = `${rule} upstream evidence`;
        const receipt = await client.beta.sessions.events.send(session.id, {
          betas: BETAS,
          events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
        });
        const receiptId = receipt.data[0]?.id;
        assert.equal(typeof receiptId, 'string', `${rule} accepted exact User Event`);
        const observed = unknown ? await waitForRetainedUnknownExecution(
          client, session.id, receiptId, BETAS, join(directory, 'store', 'dispatch.db'),
          `${rule} committed unknown diagnosis and occupied physical slot`,
        ) : await waitForSessionEventReceipt(
          client, session.id, receiptId, BETAS,
          ({ delta }) => {
            const outcome = delta.findIndex((event) => event.type === (success ? 'agent.message' : 'session.error'));
            return outcome >= 0 && delta.slice(outcome + 1).some((event) =>
              event.type === (unknown ? 'session.thread_status_idle' : 'session.status_idle'));
          },
          `${rule} committed provider outcome`,
        );
        let previous;
        for (const events of [observed.events, await listManagedSessionEvents(client, session.id, BETAS)]) {
          assert.equal(Boolean(events.find((event) => event.id === receiptId)?.processed_at), !unknown,
            `${rule} processing receipt requires physical settlement`);
          const replies = events.filter((event) => event.type === 'agent.message');
          const errors = events.filter((event) => event.type === 'session.error');
          if (success) {
            assert.equal(replies.length, 1, `${rule} exactly one reply`);
            assert.ok(replies[0].content.some((block) => block.text?.includes(`FAKE:${text}`)));
            assert.equal(errors.length, 0, `${rule} no fabricated failure`);
          } else {
            assert.equal(replies.length, 0, `${rule} no fabricated completion`);
            assert.equal(events.some((event) => event.type === 'session.status_idle'), !unknown,
              `${rule} Session availability requires positive physical termination evidence`);
            assert.equal(errors.length, 1, `${rule} exactly one stable failure`);
            assert.equal(errors[0].error.type, 'unknown_error');
            assert.equal(errors[0].error.retry_status.type, unknown ? 'terminal' : 'exhausted');
            if (unknown) assert.match(errors[0].error.message, /will not be automatically repeated/);
            if (previous) assert.deepEqual(errors[0], previous, `${rule} stable error identity and payload`);
            previous = errors[0];
          }
          assert.equal(upstream.attempts, attempts, `${rule} exact evidence-gated HTTP count`);
        }
      }, fakeAnthropicServerEnv(upstream, {
        ANTHROPIC_API_KEY: FAKE_KEY,
        SESSION_DEPLOYMENT_STORAGE_DIR: join(directory, 'store'),
        SESSION_DEPLOYMENT_SANDBOX_DIR: join(directory, 'sandboxes'),
      }));
      pass(`${rule}: exact provider outcome and evidence-gated reentry`);
    } finally {
      await upstream.close();
      cleanupFixtureTree(directory);
    }
  }
  console.log('E2E PASS: complete and interrupted HTTP outcomes gate retries and Session availability.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
