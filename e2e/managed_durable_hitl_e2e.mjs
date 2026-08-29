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
  managedAgentWithAlwaysAskTools,
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
const PORT = Number(process.env.E2E_PORT ?? 38275);
const STORE = `/tmp/awaken-durable-hitl-${process.pid}`;
async function dispatches(thread) {
  const response = await fetch(`http://127.0.0.1:${PORT}/v1/durable/threads/${thread}/dispatches`);
  assert.equal(response.status, 200, 'durable dispatch query succeeds');
  return (await response.json()).dispatches;
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  const upstream = await startUpstream('probe');
  const serverEnv = { SESSION_DEPLOYMENT_INGRESS: 'durable', SESSION_DEPLOYMENT_STORAGE_DIR: STORE, ...realServerEnv('probe', upstream) };
  let srv = null;
  let client;
  try {
    srv = spawnServer('real', PORT, serverEnv);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const s = await client.beta.sessions.create({
      agent: managedAgentWithAlwaysAskTools(['write']),
      environment_id: 'env_local',
      betas: BETAS,
    });
    const initialReceipt = await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write then read probe.txt' }] }],
      betas: BETAS,
    });

    // Cause/effect graph: C0 the Session explicitly makes write always_ask while
    // the official Agent-tool default remains allow; C1 durable first turn; C2
    // committed approval ticket; C3 exact approval input+Idempotency-Key; C4
    // resumed Run ends; C5 process restarts over the same deployment database
    // after the response could have been lost. Effects: E1 one Awaiting
    // dispatch exists before approval; E2 the dispatch Worker resumes; E3 Done
    // removes the row; E4 the Managed projection reaches end_turn; E5 an exact
    // retry returns the original receipt; E6 key reuse with another decision is
    // 409; E7 public history contains one confirmation and one tool result.
    //
    // | Rule | ticket | input | resumed state | queue effect |
    // | R1 | open | none | Awaiting | one Awaiting row |
    // | R2 | open | exact approval | Ended | row removed |
    // | R3 | consumed+restart | exact key/body | exact receipt, no new row |
    // | R4 | consumed+restart | same key/changed body | 409, no mutation |
    // The run-ingress settle suite owns the new-ticket partition.
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
    const confirmationBody = {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    };
    const confirmationOptions = { headers: { 'Idempotency-Key': 'durable-hitl-confirmation-1' } };
    const confirmationReceipt = await client.beta.sessions.events.send(
      s.id,
      confirmationBody,
      confirmationOptions,
    );
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

    // Treat the first HTTP response as lost: retain only the caller's stable key
    // and body, restart the complete server, and ask the public API again.
    await stopServer(srv.server);
    srv = null;
    srv = spawnServer('real', PORT, serverEnv);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const replay = await client.beta.sessions.events.send(
      s.id,
      confirmationBody,
      confirmationOptions,
    );
    assert.deepEqual(replay, confirmationReceipt, 'R3/E5 restart replays the exact receipt');
    await assert.rejects(
      client.beta.sessions.events.send(
        s.id,
        {
          events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'deny' }],
          betas: BETAS,
        },
        confirmationOptions,
      ),
      (error) => error?.status === 409,
      'R4/E6 same key with another approval decision conflicts',
    );
    const durableEvents = [];
    for await (const event of client.beta.sessions.events.list(s.id, { betas: BETAS })) {
      durableEvents.push(event);
    }
    assert.equal(
      durableEvents.filter((event) => event.type === 'user.tool_confirmation').length,
      1,
      'R3+R4/E7 one durable confirmation',
    );
    assert.equal(
      durableEvents.filter((event) =>
        event.type === 'agent.tool_result' && event.tool_use_id === toolUse.id).length,
      1,
      'R3+R4/E7 one durable result for the approved tool call',
    );
    pass('response-loss retry survives restart with one receipt and conflict-safe key reuse');

    console.log('E2E PASS: durable HITL — approval, restart replay, and conflict-safe idempotency form one closure.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    if (srv) await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main();
