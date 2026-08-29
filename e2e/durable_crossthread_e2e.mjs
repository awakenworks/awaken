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
// Run: (from e2e/)  node durable_crossthread_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import {
  pass,
  publishAlwaysAskManagementProbeAgent,
  spawnServer,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38180);
const BASE = `http://127.0.0.1:${PORT}`;
const STORE_DIR = `/tmp/awaken-xthread-e2e-${process.pid}`;
const THREAD = 'durable-crossthread-e2e';
const AGENT = 'durable-crossthread-agent';

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
  const srv = spawnServer('management-probe', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
  });
  await waitForPort(PORT);
  try {
    await publishAlwaysAskManagementProbeAgent(BASE, AGENT);
    // Ownership decision row X1: generic durable ingress owns this ordinary
    // Runtime Thread; Managed Session roots are excluded and use Session-owned
    // reservations. One published fixture Agent explicitly sets write=AlwaysAsk;
    // the daemon therefore drains this Run to an awaiting permission boundary.
    const sub = await post(`/v1/durable/threads/${THREAD}/submit_background`, {
      agent: AGENT,
      text: 'XTHREAD',
    });
    assert.equal(sub.status, 200, 'background submit accepted');
    pass('run background-submitted; daemon will drain it to an awaiting tool');

    // Stage a cross-thread decision into the outbox once the run has awaiting. The
    // daemon relays it from the outbox and wakes the run.
    let staged = false;
    for (let i = 0; i < 100; i++) {
      const res = await post(`/v1/durable/threads/${THREAD}/deliver`, { allow: true });
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
      const { messages } = await committed(THREAD);
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
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
