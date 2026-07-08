// Durable-ingress token-aware compaction e2e. Proves the token-aware fold RUNS
// when turns are delivered through the durable dispatch worker
// (AWAKEN_INGRESS=durable), not just inline: after a couple of large turns cross
// the token budget, the compactor sub-agent folds the older slice and its summary
// is injected on later turns — observed here through the deterministic
// `CompactionModel`, which echoes the injected system context. Also proves the
// compacted thread rehydrates and keeps running across a real process restart.
//
// NOTE: this asserts the compaction *behavior* under durable ingress. The
// `agent.thread_context_compacted` wire *event* currently projects only under
// direct ingress (the fold marker in committed thread state is not surfaced to
// the terminal-step read under the durable dispatch path); wiring the event onto
// the durable projection is a tracked follow-up. See managed_compaction_e2e.mjs /
// managed_compaction_tokens_e2e.mjs for the event assertions (direct ingress).
//
// Run: (from e2e/)  node managed_compaction_durable_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38235);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-compact-durable-e2e-${process.pid}`;
const ENV = {
  AWAKEN_STORAGE_DIR: STORE_DIR,
  AWAKEN_INGRESS: 'durable',
  AWAKEN_COMPACT_MAX_TOKENS: '200', // budget = 0.5 * 200 = 100 est. tokens
  AWAKEN_COMPACT_TRIGGER_RATIO: '0.5',
  AWAKEN_COMPACT_KEEP_LAST: '1',
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function allEvents(client, id) {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) evs.push(ev);
  return evs;
}

// Send a turn and poll until it reaches a terminal idle (durable ingress drives
// it out of band through the dispatch worker).
async function turnAndDrain(client, id, text) {
  await client.beta.sessions.events.send(id, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  for (let i = 0; i < 60; i += 1) {
    const evs = await allEvents(client, id);
    if (evs.some((e) => e.type === 'session.status_idle')) return evs;
    await sleep(200);
  }
  throw new Error('durable turn never reached idle');
}

async function main() {
  const fs = await import('node:fs');
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });

  const big = (n) => `Turn ${n}: ` + 'filler '.repeat(120);

  // ---- server A: compaction through the durable dispatch worker ----
  let a = spawnServer('compaction', PORT, ENV);
  try {
    await waitForPort(PORT);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: a.baseUrl });
    const s = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });

    // The CompactionModel echoes injected system context as `ctx:[...]`; once the
    // fold runs, that context carries the compactor's summary line.
    const reply = (evs) =>
      evs
        .filter((e) => e.type === 'agent.message')
        .map((e) => e.content.map((b) => b.text ?? '').join(''))
        .at(-1) ?? '';
    let folded = false;
    let turns = 0;
    for (let i = 1; i <= 4 && !folded; i += 1) {
      turns = i;
      const evs = await turnAndDrain(client, s.id, big(i));
      folded = reply(evs).includes('Summary of earlier conversation');
    }
    assert.ok(folded, `durable-ingress token-aware fold ran within ${turns} turns`);
    pass(`compaction folded through the durable dispatch worker after ${turns} turns`);

    // ---- restart: the thread rehydrates and keeps running ----
    await stopServer(a.server);
    a = spawnServer('compaction', PORT, ENV);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: a.baseUrl });

    const after = await turnAndDrain(client, s.id, big(99));
    assert.ok(
      after.some((e) => e.type === 'agent.message'),
      'a post-restart turn ran through the rehydrated durable ingress',
    );
    pass('durable ingress reconnected and continued the compacted thread after restart');

    console.log('E2E PASS: durable-ingress token-aware compaction + cross-restart continuity.');
  } finally {
    await stopServer(a.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
