// Durable HITL — the DENY path through the dispatch worker, plus exactly-once on
// resume. The sibling `managed_durable_hitl_e2e.mjs` proves only APPROVE durably;
// this proves the other half: under AWAKEN_INGRESS=durable a run awaits on a tool
// needing approval, the client DENIES it, and the DISPATCH WORKER resumes the
// awaiting run — the tool effect is refused (probe.txt is never written, so the
// read-back does not contain the text) yet the run still drives to a terminal
// end_turn. We also assert exactly-once: the worker drives each tool exactly once
// (no duplicate tool_use ids) and a re-delivered confirmation after the run ended
// does NOT re-drive the run (idempotent resume — no double-drive).
//
// Deterministic, CI-safe (probe model). Run: node e2e/durable_worker_hitl_deny_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 39717;
const STORE = `/tmp/awaken-durable-hitl-deny-${process.pid}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function listEvents(client, id) {
  const e = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) e.push(ev);
  return e;
}
async function until(fn) {
  for (let i = 0; i < 150; i++) {
    const v = await fn();
    if (v) return v;
    await sleep(50);
  }
  return null;
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORE,
    ...realServerEnv('probe', upstream),
  });
  await waitForPort(PORT);
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
  try {
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const MARK = 'DENY-THIS-WRITE';
    await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: MARK }] }],
      betas: BETAS,
    });

    // The durable run awaits on the write tool_use awaiting approval.
    const toolUse = await until(async () => (await listEvents(client, s.id)).find((e) => e.type === 'agent.tool_use'));
    assert.ok(toolUse, 'the durable run awaiting on a tool_use awaiting approval');
    assert.equal(toolUse.evaluated_permission, 'ask', 'the mutating write tool awaiting with an ask gate');
    const awaitingIdle = (await listEvents(client, s.id)).find((e) => e.type === 'session.status_idle');
    assert.equal(awaitingIdle.stop_reason.type, 'requires_action', 'run awaiting (requires_action) in the durable queue');
    pass('durable run awaiting on the write tool_use (requires_action)');

    // DENY — the DISPATCH WORKER resumes the awaiting durable run and refuses the tool.
    await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'deny', deny_message: 'not allowed' }],
      betas: BETAS,
    });

    // The worker resumes and drives the run to a terminal end_turn.
    const ended = await until(async () => {
      const evs = await listEvents(client, s.id);
      const lastIdle = [...evs].reverse().find((e) => e.type === 'session.status_idle');
      return lastIdle && lastIdle.stop_reason.type === 'end_turn' ? evs : null;
    });
    assert.ok(ended, 'the dispatch worker resumed the denied run and drove it to end_turn');
    pass('durable ingress: worker resumed the awaiting run after DENY and reached end_turn');

    // The effect is refused: probe.txt was never written, so the read-back tool
    // result does NOT contain the marker text.
    const results = ended.filter((e) => e.type === 'agent.tool_result');
    assert.ok(
      !JSON.stringify(results.map((r) => r.content)).includes(MARK),
      'DENY blocked the write — the read-back never sees the marker text (effect refused)',
    );
    pass('DENY refused the tool effect: write blocked, read-back has no marker');

    // Exactly-once (no double-drive): the write tool_use appears exactly once, and
    // there is exactly one tool_result per distinct tool_use.
    const toolUses = ended.filter((e) => e.type === 'agent.tool_use');
    const useIds = toolUses.map((e) => e.id);
    assert.equal(new Set(useIds).size, useIds.length, 'no duplicate agent.tool_use ids (worker drove each tool once)');
    assert.equal(useIds.filter((id) => id === toolUse.id).length, 1, 'the write tool was driven exactly once');
    pass(`exactly-once: worker drove ${useIds.length} distinct tool_use(s), none repeated`);

    // Idempotent resume: re-delivering the SAME confirmation after the run ended
    // must NOT re-drive the run. The adapter fails closed (no pending tool), and
    // committed truth is unchanged — the worker never resumes a finished run twice.
    const before = await listEvents(client, s.id);
    let reDelivered = false;
    try {
      await client.beta.sessions.events.send(s.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'deny', deny_message: 'again' }],
        betas: BETAS,
      });
      reDelivered = true;
    } catch {
      // Fail-closed at the adapter (no pending tool) is the expected rejection.
    }
    await sleep(300);
    const after = await listEvents(client, s.id);
    assert.equal(after.length, before.length, 'a re-delivered confirmation did not re-drive the run (exactly-once)');
    pass(`re-delivered confirmation was ${reDelivered ? 'accepted-but-inert' : 'rejected'} — no double-drive on resume`);

    console.log('E2E PASS: durable HITL DENY — worker resumes, refuses the effect, ends the run, exactly-once.');
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
