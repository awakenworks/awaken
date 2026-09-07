// Real Anthropic HTTP -> adapter evidence -> durable Run -> public Session.
// Run: (from e2e/) node managed_upstream_faults_e2e.mjs
import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { fakeAnthropicServerEnv, withServer, pass, waitForSessionEventReceipt } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-faults-key'; // awaken-allow: secret

async function main() {
  // Cause-effect decision table: C1 protocol-complete success / HTTP 503;
  // C2 the fixture would succeed on a later request / always rejects;
  // C3 an accepted exact receipt and repeated public reads.
  // F1 success => one request, one exact reply, no error (positive control).
  // F2 503 + later success / F3 503 + persistent rejection => one request,
  // no reply, one stable unknown_error/terminal before logical Thread idle;
  // Session remains occupied without physical termination proof. C2 cannot supply
  // remote termination evidence, so neither retry nor retry exhaustion is valid.
  // C3 proves these are committed outcomes, not an HTTP admission rejection.
  // Runtime failure.rs owns retries with positive NotDispatched evidence;
  // execution.rs covers Unknown across retry/failover/tolerance and recovery.
  // The Kani model_execution_reentry_requires_positive_evidence kernel owns
  // the exhaustive three-valued gate; this HTTP test checks its adapter wiring.
  for (const [rule, options, success] of [
    ['F1', {}, true],
    ['F2', { failuresBeforeSuccess: 1 }, false],
    ['F3', { alwaysFail: true }, false],
  ]) {
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
        const observed = await waitForSessionEventReceipt(
          client, session.id, receiptId, BETAS,
          ({ delta }) => {
            const outcome = delta.findIndex((event) => event.type === (success ? 'agent.message' : 'session.error'));
            return outcome >= 0 && delta.slice(outcome + 1).some((event) =>
              event.type === (success ? 'session.status_idle' : 'session.thread_status_idle'));
          },
          // Unknown retains the physical slot; Session's canonical 30-second
          // reconciliation sweep may publish the receipt. Allow two sweeps;
          // this is a safety/convergence oracle, not the K3d latency gate.
          `${rule} committed provider outcome`, { timeoutMs: 60_000 },
        );
        let previous;
        for (const events of [observed.events, (await client.beta.sessions.events.list(session.id, { betas: BETAS })).data]) {
          const replies = events.filter((event) => event.type === 'agent.message');
          const errors = events.filter((event) => event.type === 'session.error');
          if (success) {
            assert.equal(replies.length, 1, `${rule} exactly one reply`);
            assert.ok(replies[0].content.some((block) => block.text?.includes(`FAKE:${text}`)));
            assert.equal(errors.length, 0, `${rule} no fabricated failure`);
          } else {
            assert.equal(replies.length, 0, `${rule} no fabricated completion`);
            assert.ok(!events.some((event) => event.type === 'session.status_idle'),
              `${rule} logical Thread end cannot advertise physical Session availability`);
            assert.equal(errors.length, 1, `${rule} exactly one uncertainty diagnosis`);
            assert.equal(errors[0].error.type, 'unknown_error');
            assert.equal(errors[0].error.retry_status.type, 'terminal');
            assert.match(errors[0].error.message, /will not be automatically repeated/);
            if (previous) assert.deepEqual(errors[0], previous, `${rule} stable error identity and payload`);
            previous = errors[0];
          }
          assert.equal(upstream.attempts, 1, `${rule} no hidden HTTP or Runtime retry`);
        }
      }, fakeAnthropicServerEnv(upstream, { ANTHROPIC_API_KEY: FAKE_KEY }));
      pass(`${rule}: exact provider outcome and evidence-gated reentry`);
    } finally {
      upstream.close();
    }
  }
  console.log('E2E PASS: complete success and ambiguous upstream failures preserve execution evidence.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
