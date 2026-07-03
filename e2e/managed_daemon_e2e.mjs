// Autonomous dispatch daemon end-to-end (slice E follow-up, ADR-0011) via the
// durable operations surface plus the Anthropic TS SDK.
//
// With AWAKEN_INGRESS=durable + AWAKEN_DISPATCH_DAEMON=1 each durable session runs
// a standing `DispatchService` daemon. We POST a run to
// /v1/durable/threads/:t/submit_background: it is durably ENQUEUED and the call
// returns immediately — no foreground request drives it. The daemon then drains
// the queue on its own (nudge + timer), executes the run, and commits the reply.
// We observe completion purely by polling committed truth, proving the daemon —
// not the request — did the work.
//
// Run: (from e2e/)  node managed_daemon_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38179);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-daemon-e2e-${process.pid}`;
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

// Committed truth (not the session's in-memory event log): the only channel that
// reflects an out-of-band, daemon-drained run.
const committed = async (thread) => {
  const res = await fetch(`${BASE}/v1/durable/threads/${thread}/messages`);
  return res.json();
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const srv = spawnServer('echo', PORT, {
    AWAKEN_STORAGE_DIR: STORE_DIR,
    AWAKEN_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
  });
  await waitForPort(PORT);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

    // Enqueue for the daemon. The call returns immediately — the run is queued,
    // not driven by this request.
    const res = await fetch(`${BASE}/v1/durable/threads/${session.id}/submit_background`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ text: 'DAEMON-DRAIN' }),
    });
    const body = await res.json();
    assert.equal(res.status, 200, 'background submit accepted');
    assert.equal(body.queued, true, 'run was queued, not driven inline');
    assert.ok(body.run_id, 'a run id was assigned');
    pass(`run ${body.run_id} durably enqueued; request returned without driving it`);

    // Poll committed truth until the daemon executes and commits the reply. The
    // reply is committed out of band — no foreground request drives it.
    let assistant = [];
    for (let i = 0; i < 100; i++) {
      const { messages } = await committed(session.id);
      assistant = messages.filter((m) => m.role === 'Assistant' && m.text.includes('DAEMON-DRAIN'));
      if (assistant.length >= 1) break;
      await sleep(50);
    }
    assert.ok(assistant.length >= 1, 'the daemon drained the queued run and committed its reply');
    assert.ok(assistant[0].text.includes('Echo: DAEMON-DRAIN'), 'the daemon-driven run produced the echo reply');
    pass('the standing dispatch daemon drained the queued run out of band (ADR-0011)');

    console.log('E2E PASS: autonomous dispatch daemon drains a background-submitted run (ADR-0011).');
  } finally {
    await stopServer(srv.server);
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
