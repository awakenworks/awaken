// Durable outbox / cross-thread delivery end-to-end (slice E follow-up, ADR-0017)
// via the durable operations surface plus the standing dispatch daemon.
//
// A run is background-submitted and the daemon drains it; the probe model's write
// tool awaits for confirmation, so the run sits `awaiting` in the durable queue. We
// then POST /v1/durable/threads/:t/deliver: this STAGES a decision into the
// durable OUTBOX (not a direct resume). The daemon relays the staged delivery from
// the outbox to the awaiting run's pending input and wakes it — the run resumes,
// reads back, and completes. This exercises the outbox stage → relay → wake path.
//
// Run: (from e2e/)  node managed_crossthread_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38180);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-xthread-e2e-${process.pid}`;
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

const post = (path, body) =>
  fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body ?? {}),
  });
const committed = (thread) => fetch(`${BASE}/v1/durable/threads/${thread}/messages`).then((r) => r.json());
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, {
    AWAKEN_STORAGE_DIR: STORE_DIR,
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
    ...realServerEnv('probe', upstream),
  });
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // Background-submit: the daemon drains it and the probe's write tool awaits.
    const sub = await post(`/v1/durable/threads/${session.id}/submit_background`, { text: 'XTHREAD' });
    assert.equal(sub.status, 200, 'background submit accepted');
    pass('run background-submitted; daemon will drain it to an awaiting tool');

    // Stage a cross-thread decision into the outbox once the run has awaiting. The
    // daemon relays it from the outbox and wakes the run.
    let staged = false;
    for (let i = 0; i < 100; i++) {
      const res = await post(`/v1/durable/threads/${session.id}/deliver`, { allow: true });
      if (res.status === 200) {
        staged = true;
        break;
      }
      await sleep(50);
    }
    assert.ok(staged, 'a decision was staged into the outbox for the awaiting run');
    pass('decision staged into the durable outbox (ADR-0017)');

    // The daemon relays the staged delivery and the run resumes to completion.
    let done = false;
    for (let i = 0; i < 100; i++) {
      const { messages } = await committed(session.id);
      if (messages.some((m) => m.role === 'Assistant' && m.text.includes('done'))) {
        done = true;
        break;
      }
      await sleep(50);
    }
    assert.ok(done, 'the daemon relayed the outbox delivery and the run resumed to completion');
    pass('outbox delivery relayed → awaiting run woken and completed (ADR-0017)');

    console.log('E2E PASS: durable outbox cross-thread delivery relayed by the daemon (ADR-0017).');
  } finally {
    await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
