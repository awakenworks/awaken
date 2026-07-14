// Durable cross-protocol resume e2e (scenario #49): with durable ingress
// (AWAKEN_INGRESS=durable + AWAKEN_STORAGE_DIR), a run PARKS on a tool approval on
// the AI-SDK wire and is APPROVED + resumed on the AG-UI wire. The resume is not a
// foreground inline execution — it flows through DurableRunIngress.deliver_resume
// and the DISPATCH WORKER drives the parked run to completion. Same thread id, same
// durable `SharedHost`, different wire.
//
// Chain:
//   AI-SDK : POST /v1/ai-sdk/threads/T/runs -> SharedHost (durable) ->
//            submit_background -> DispatchPool -> Runtime (probe write) -> park (persisted)
//   AG-UI  : POST /v1/ag-ui/agents/assistant (role:"tool" approve) ->
//            deliver_resume -> DispatchWorker resume branch -> write executes -> done
//   AI-SDK : GET history -> completed, write took effect (read-back present)
//
// Deterministic (probe stub). Run: (from e2e/) node durable_cross_protocol_resume_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38608);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function drain(res) {
  const raw = await res.text();
  const events = [];
  for (const line of raw.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('data:')) continue;
    const p = t.slice(5).trim();
    if (!p || p === '[DONE]') continue;
    try {
      events.push(JSON.parse(p));
    } catch {
      /* ignore */
    }
  }
  return events;
}

async function history(base, thread) {
  const res = await fetch(`${base}/v1/ai-sdk/threads/${thread}/messages`);
  assert.equal(res.status, 200, 'history read ok');
  return (await res.json()).items;
}

async function until(fn, tries = 150) {
  for (let i = 0; i < tries; i++) {
    const v = await fn();
    if (v) return v;
    await sleep(50);
  }
  return null;
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-durable-xproto-'));
  const { server, baseUrl: base } = spawnServer('probe', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: dir,
  });
  try {
    await waitForPort(PORT);
    const thread = `dur-xproto-${randomBytes(4).toString('hex')}`;
    const NOTE = `DUR-${randomBytes(4).toString('hex')}`;

    // --- Turn 1 on AI-SDK under durable ingress: parks --------------------
    const r1 = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ threadId: thread, messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: NOTE }] }] }),
    });
    assert.equal(r1.status, 200, 'ai-sdk durable turn accepted');
    const events = await drain(r1);
    // The parked tool-call id: from the stream if present, else recovered from the
    // committed (persisted) history — durable may degrade the live stream.
    let toolCallId = events.find((e) => e.toolCallId)?.toolCallId;
    if (!toolCallId) {
      const items = await until(async () => {
        const h = await history(base, thread);
        const raw = JSON.stringify(h);
        return raw.includes('write') ? h : null;
      });
      assert.ok(items, 'the parked write is persisted in durable history');
      // The probe write tool call id is the stable 'w'.
      toolCallId = 'w';
    }
    // Confirm the run is genuinely parked (not yet completed) in durable state.
    const parkedHist = JSON.stringify(await history(base, thread));
    assert.ok(!parkedHist.includes('done'), 'durable run parked (not completed) before approval');
    pass(`durable run parked on AI-SDK (persisted under ${path.basename(dir)}, toolCallId=${toolCallId})`);

    // --- Approve on AG-UI: the dispatch worker resumes the durable run ----
    const r2 = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: thread,
        runId: `run-${randomBytes(3).toString('hex')}`,
        messages: [{ id: 'tr1', role: 'tool', content: 'approved', toolCallId }],
        tools: [],
        context: [],
        state: {},
        forwardedProps: {},
      }),
    });
    assert.equal(r2.status, 200, `ag-ui durable resume accepted (${r2.status})`);
    await drain(r2);
    pass('AG-UI delivered the approval; DurableRunIngress.deliver_resume drove the parked run');

    // --- The durable run completed and the approved write executed --------
    const done = await until(async () => {
      const raw = JSON.stringify(await history(base, thread));
      return raw.includes('done') ? raw : null;
    });
    assert.ok(done, 'the durable cross-protocol resume reached completion');
    const occurrences = done.split(NOTE).length - 1;
    assert.ok(
      occurrences >= 3,
      `the approved write executed under durable resume (read-back present, occurrences=${occurrences})`,
    );
    pass('durable cross-protocol HITL: parked on AI-SDK, approved on AG-UI, worker resumed to completion');
  } finally {
    await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: durable cross-protocol resume (AI-SDK park -> AG-UI approve -> dispatch worker resume).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
