// Durable HITL: under SESSION_DEPLOYMENT_INGRESS=durable a run awaits on a tool needing
// approval, and after the client sends the confirmation the DISPATCH WORKER
// resumes the awaiting run (not a foreground request). Drives the durable resume
// path — engine::resume_run / resume_into_messages, SharedHost::resume, and the
// worker's resume branch — that the direct-ingress HITL e2e does not reach.
// Deterministic, CI-safe (probe model).

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38275;
const STORE = `/tmp/awaken-durable-hitl-${process.pid}`;
async function dispatches(thread) {
  const response = await fetch(`http://127.0.0.1:${PORT}/v1/durable/threads/${thread}/dispatches`);
  assert.equal(response.status, 200, 'durable dispatch query succeeds');
  return (await response.json()).dispatches;
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, { SESSION_DEPLOYMENT_INGRESS: 'durable', SESSION_DEPLOYMENT_STORAGE_DIR: STORE, ...realServerEnv('probe', upstream) });
  await waitForPort(PORT);
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
  try {
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const initialReceipt = await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write then read probe.txt' }] }],
      betas: BETAS,
    });

    // Cause/effect graph: C1 durable first turn; C2 committed approval ticket;
    // C3 exact approval input; C4 resumed Run ends. Effects: E1 one Awaiting
    // dispatch exists before approval; E2 the dispatch Worker resumes; E3 Done
    // removes the row; E4 the Managed projection reaches end_turn.
    //
    // | Rule | ticket | input | resumed state | queue effect |
    // | R1 | open | none | Awaiting | one Awaiting row |
    // | R2 | open | exact approval | Ended | row removed |
    // | R3 | open | exact approval | Awaiting(new ticket) | one Awaiting row |
    // This scenario covers R1/R2. The run-ingress settle suite owns R3.
    // The durable run awaits on a tool_use awaiting approval.
    const initialReceiptId = initialReceipt.data[0]?.id;
    assert.equal(typeof initialReceiptId, 'string', 'R1 exact durable User Event receipt');
    const awaiting = await waitForSessionEventReceipt(
      client,
      s.id,
      initialReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_use')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'requires_action',
      'R1 durable Run to commit its awaiting ticket',
    );
    const toolUse = awaiting.delta.find((event) => event.type === 'agent.tool_use');
    assert.ok(toolUse, 'the durable run awaiting on a tool_use awaiting approval');
    const waitingRows = await waitForValue(
      () => dispatches(s.id),
      (rows) => rows.some((row) => row.status === 'Awaiting'),
      'R1 durable dispatch to project Awaiting',
    );
    assert.equal(waitingRows.filter((row) => row.status === 'Awaiting').length, 1, 'R1/E1: one awaiting dispatch');
    pass('durable run awaiting on a tool_use (requires_action)');

    // Approve — the DISPATCH WORKER resumes the awaiting durable run out of band.
    const confirmationReceipt = await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    });
    const confirmationReceiptId = confirmationReceipt.data[0]?.id;
    assert.equal(typeof confirmationReceiptId, 'string', 'R2 exact approval receipt');
    await waitForSessionEventReceipt(
      client,
      s.id,
      confirmationReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_result')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'R2 durable Worker to resume and commit after approval',
    );
    assert.equal((await dispatches(s.id)).length, 0, 'R2/E3: terminal resume removes the durable dispatch');
    pass('durable ingress: an awaiting run resumed by the worker after approval (resume path)');

    console.log('E2E PASS: durable HITL — the dispatch worker resumes an awaiting run after approval.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main();
