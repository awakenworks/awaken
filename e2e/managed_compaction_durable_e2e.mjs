// Durable-ingress token-aware compaction e2e. Proves the token-aware fold RUNS
// when turns are delivered through the durable dispatch worker
// (SESSION_DEPLOYMENT_INGRESS=durable), not just inline: after a couple of large turns cross
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
import { spawnServer, stopServer, waitForPort, pass, waitForSessionEventReceipt } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38235);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-compact-durable-e2e-${process.pid}`;
const ENV = {
  SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
  SESSION_DEPLOYMENT_INGRESS: 'durable',
  AWAKEN_COMPACT_MAX_TOKENS: '200', // budget = 0.5 * 200 = 100 est. tokens
  AWAKEN_COMPACT_TRIGGER_RATIO: '0.5',
  AWAKEN_COMPACT_KEEP_LAST: '1',
};

async function turnAndDrain(client, id, text) {
  // C1=exact durable receipt; C2=worker commits reply+terminal. E1=full history
  // after C2. K: observation never drives the dispatch Worker. Decision D1
  // C1&&!C2=>retry; D2 C1+C2=>return committed history.
  const receipt = await client.beta.sessions.events.send(id, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'D1 exact durable-compaction User Event receipt');
  const { events } = await waitForSessionEventReceipt(
    client,
    id,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `D1 durable-compaction Run for ${JSON.stringify(text)} to commit`,
  );
  return events;
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
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

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
