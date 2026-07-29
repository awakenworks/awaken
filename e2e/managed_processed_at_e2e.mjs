// §6.2 — the processed_at queued→committed gate, via the official Anthropic TS SDK
// against awaken-server (echo model).
//
// A client drives "pending -> acknowledged" UI off processed_at. In this server the
// distinction is observable across two surfaces: the POST .../events RECEIPT carries
// processed_at:null for each just-queued message, while its same-id persisted event
// and every generated event returned by events.list carry a non-null timestamp.
//
// Run: (from e2e/)  node managed_processed_at_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

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

      // Cause graph: a standard queued user.message yields a null receipt timestamp;
      // successful processing yields the same id in history with a timestamp. The
      // three messages also prove request ordering, rather than one lucky id match.
      const receipts = [];
      for (const text of ['one', 'two', 'three']) {
        const receipt = await client.beta.sessions.events.send(session.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
          betas: BETAS,
        });
        assert.ok(Array.isArray(receipt.data) && receipt.data.length === 1, 'one receipt per queued event');
        const r = receipt.data[0];
        assert.equal(r.type, 'user.message', 'the receipt echoes the queued event type');
        assert.ok(r.id, 'the receipt assigns an event id');
        assert.equal(r.processed_at, null, 'a just-queued event is acknowledged with processed_at: null');
        receipts.push(r);
      }
      pass('every events.send receipt carries processed_at: null (queued/acknowledged)');

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

    console.log('E2E PASS: processed_at queued(null on receipt) -> committed(timestamp in list) via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
