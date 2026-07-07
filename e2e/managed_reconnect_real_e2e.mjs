// Real-model reconnect conformance for Managed Agents, driven by the official
// Anthropic TypeScript SDK against awaken-server in `real` mode (backed by a
// live Anthropic-compatible model — the KIMI_*/ANTHROPIC_* env config).
//
// The point echo mode can't make: with a real model a turn takes time, so we can
// drop the SSE stream WHILE the turn is still in flight and prove that a client
// which reconnects (reopen stream + events.list(), dedupe by id) still recovers
// the whole turn — the missed events are filled from the authoritative history.
//
// Run: (from e2e/, with the KIMI config from ~/.bashrc — note the base needs the
// trailing /v1/, the genai anthropic adapter appends `messages` to it)
//   ANTHROPIC_API_KEY=sk-kimi-... \
//   ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-k2-0711-preview \
//   node managed_reconnect_real_e2e.mjs
// `real` mode also accepts the KIMI_API_KEY/KIMI_BASE_URL/KIMI_MODEL aliases.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38141);

async function listAll(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_reconnect_real_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      pass(`session created: ${session.id}`);

      // Open the stream, kick off a real turn, then DROP the stream after the very
      // first event — the model is still working, so later events are missed here.
      const dropped = await client.beta.sessions.events.stream(session.id, { betas: BETAS });
      await client.beta.sessions.events.send(session.id, {
        events: [
          { type: 'user.message', content: [{ type: 'text', text: 'In one short sentence, say hello.' }] },
        ],
        betas: BETAS,
      });
      const seenBeforeDrop = new Set();
      for await (const ev of dropped) {
        if (ev.id) seenBeforeDrop.add(ev.id);
        break; // simulate a mid-turn disconnect
      }
      pass(`dropped the stream after ${seenBeforeDrop.size} event(s), turn still in flight`);

      // Reconnect: reopen the stream and consolidate with the authoritative history,
      // deduping by id, draining until the turn is genuinely done.
      const reopened = await client.beta.sessions.events.stream(session.id, { betas: BETAS });
      const merged = new Map();
      for (const ev of await listAll(client, session.id)) merged.set(ev.id, ev);
      for await (const ev of reopened) {
        if (ev.id) merged.set(ev.id, ev);
        if (ev.type === 'session.status_idle' && ev.stop_reason?.type !== 'requires_action') break;
        if (ev.type === 'session.status_terminated') break;
      }
      // Final backfill from history in case idle raced ahead of the live tail.
      for (const ev of await listAll(client, session.id)) merged.set(ev.id, ev);

      const events = [...merged.values()];
      const agentMsg = events.find((e) => e.type === 'agent.message');
      assert.ok(agentMsg, `consolidation recovered the agent.message (types: ${events.map((e) => e.type)})`);
      const text = (agentMsg.content ?? []).map((c) => c.text ?? '').join('').trim();
      assert.ok(text.length > 0, 'the recovered turn carries the real model reply');
      assert.ok(
        events.some((e) => e.type === 'session.status_idle'),
        'the recovered turn reached a terminal idle',
      );
      pass(`recovered the dropped turn via list+stream consolidation: ${JSON.stringify(text.slice(0, 80))}`);
    });

    console.log('E2E PASS: managed reconnect recovers a real model turn dropped mid-flight via the TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
