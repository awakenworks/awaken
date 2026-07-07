// §6.2 — the processed_at queued→committed gate, via the official Anthropic TS SDK
// against awaken-server-local (echo model).
//
// A client drives "pending -> acknowledged" UI off processed_at. In this server the
// distinction is observable across two surfaces: the POST .../events RECEIPT carries
// processed_at:null for each just-queued event, while every COMMITTED event returned
// by events.list carries a non-null timestamp. This locks both halves so a receipt
// that started stamping (or a list that started returning null) is caught.
//
// Run: (from e2e/)  node managed_processed_at_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38402);

async function main() {
  try {
    await withServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });

      // Each send returns a receipt whose queued events carry processed_at: null.
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
      // The committed history is outbound only (agent.* / session.*), the inbound
      // acknowledgements live on the receipts above — assert that separation too.
      assert.ok(
        events.every((e) => e.type.startsWith('agent.') || e.type.startsWith('session.')),
        `list() returns committed outbound events only (got ${events.map((e) => e.type)})`,
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
