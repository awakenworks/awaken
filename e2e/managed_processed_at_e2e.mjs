// §6.2 — the processed_at queued→committed gate, via the official Anthropic TS SDK
// against awaken-server (echo model).
//
// A client drives "pending -> acknowledged" UI off processed_at. The POST
// .../events receipt may race the Run commit and therefore expose null or the
// timestamp; authoritative history must converge on the same id with a non-null
// timestamp, and every generated committed Event must also be timestamped.
//
// Run: (from e2e/)  node managed_processed_at_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { waitForSessionEventReceipt, withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38402);

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });

      // Cause/effect graph: C1=a User event is durably accepted; C2=its Run has
      // not committed before the HTTP receipt projection; C3=its Run has already
      // committed before that projection. Effects: E1=the receipt owns a stable
      // id; E2=C2 exposes processed_at:null and later history timestamps the same
      // id; E3=C3 may already expose that timestamp; E4=history is ordered and
      // every committed Event is timestamped. Decision table: P1(C1+C2)->E1+E2;
      // P2(C1+C3)->E1+E3; P3(three sequential P1/P2 cases)->E4. The test waits
      // between inputs so it measures receipt/commit races, not awaiting-Run
      // admission, which is owned by the event-batch E2E. Constraints/invariant:
      // receipt/history identity is stable and committed Events never retain a
      // null processed_at.
      const receipts = [];
      for (const [index, text] of ['one', 'two', 'three'].entries()) {
        const receipt = await client.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
          betas: BETAS,
        });
        assert.ok(Array.isArray(receipt.data) && receipt.data.length === 1, 'one receipt per queued event');
        const r = receipt.data[0];
        assert.equal(r.type, 'user.message', 'the receipt echoes the queued event type');
        assert.ok(r.id, 'the receipt assigns an event id');
        assert.ok(
          r.processed_at === null || typeof r.processed_at === 'string',
          'P1/P2 receipt is either queued or already committed, never ambiguous',
        );
        receipts.push(r);
        const { events: committed } = await waitForSessionEventReceipt(
          client,
          session.id,
          r.id,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'agent.message'),
          `P${index + 1} receipt id to converge in committed history`,
        );
        const persisted = committed.find((event) => event.id === r.id);
        if (r.processed_at !== null) {
          assert.equal(persisted.processed_at, r.processed_at, 'P2 receipt and history share the commit timestamp');
        }
      }
      pass('each events.send receipt converges by stable id across the queued/committed race');

      // Every committed event in the authoritative history carries a real timestamp.
      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
      assert.ok(events.length > 0, 'the run produced committed events');
      for (const ev of events) {
        assert.ok(
          typeof ev.processed_at === 'string' && ev.processed_at.length > 0,
          `committed ${ev.type} carries a processed_at timestamp (got ${JSON.stringify(ev.processed_at)})`,
        );
      }
      const persistedInputs = events.filter((event) => event.type === 'user.message');
      assert.deepEqual(
        persistedInputs.map((event) => event.id),
        receipts.map((receipt) => receipt.id),
        'queued receipts and persisted inputs share ids and request order',
      );
      pass('every committed event in events.list() carries a non-null processed_at');
    });

    console.log('E2E PASS: processed_at receipt race -> committed same-id history via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
