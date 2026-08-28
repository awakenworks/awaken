// Durable HITL — the DENY path through the dispatch worker, plus exactly-once on
// resume. The sibling `managed_durable_hitl_e2e.mjs` proves only APPROVE durably;
// this proves the other half: under SESSION_DEPLOYMENT_INGRESS=durable a run awaits on a tool
// needing approval, the client DENIES it, and the DISPATCH WORKER resumes the
// awaiting run — the tool effect is refused (probe.txt is never written, so the
// read-back does not contain the text) yet the run still drives to a terminal
// end_turn. We also assert exactly-once: the worker drives each tool exactly once
// (no duplicate tool_use ids).
//
// Deterministic, CI-safe (probe model). Run: node e2e/durable_worker_hitl_deny_e2e.mjs

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
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 39717;
const STORE = `/tmp/awaken-durable-hitl-deny-${process.pid}`;
async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  const upstream = await startUpstream('probe');
  let srv = null;
  try {
    srv = spawnServer('real', PORT, {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: STORE,
      ...realServerEnv('probe', upstream),
    });
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const s = await client.beta.sessions.create({
      agent: {
        id: 'assistant',
        type: 'agent_with_overrides',
        tools: [{
          type: 'agent_toolset_20260401',
          configs: [{
            name: 'write',
            type: 'write',
            enabled: true,
            permission_policy: { type: 'always_ask' },
          }],
        }],
      },
      environment_id: 'env_local',
      betas: BETAS,
    });
    const MARK = 'DENY-THIS-WRITE';
    // C0=the Session explicitly makes write always_ask while read inherits the
    // official allow default; C1=exact User receipt; C2=Awaiting ticket;
    // C3=exact deny receipt; C4=end_turn without the denied effect. E1=C2 is
    // after C1; E2=C4 is after C3. K: a historical idle/result is ineligible.
    // Decision D1 C0+C1&&!C2=>retry; D2 C1+C2+C3&&!C4=>retry; D3 all=>assert
    // denial, linked read-back, and exactly-once.
    const initialReceipt = await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: MARK }] }],
      betas: BETAS,
    });

    // The durable run awaits on the write tool_use awaiting approval.
    const initialReceiptId = initialReceipt.data[0]?.id;
    assert.equal(typeof initialReceiptId, 'string', 'D1 exact durable User Event receipt');
    const awaiting = await waitForSessionEventReceipt(
      client,
      s.id,
      initialReceiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_use')
        && [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'requires_action',
      'D1 durable Run to commit its awaiting ticket',
    );
    const toolUse = awaiting.delta.find((event) => event.type === 'agent.tool_use');
    assert.ok(toolUse, 'the durable run awaiting on a tool_use awaiting approval');
    assert.equal(toolUse.evaluated_permission, 'ask', 'the mutating write tool awaiting with an ask gate');
    const awaitingIdle = awaiting.delta.find((e) => e.type === 'session.status_idle');
    assert.equal(awaitingIdle.stop_reason.type, 'requires_action', 'run awaiting (requires_action) in the durable queue');
    pass('durable run awaiting on the write tool_use (requires_action)');

    // DENY — the DISPATCH WORKER resumes the awaiting durable run and refuses the tool.
    const denyReceipt = await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'deny', deny_message: 'not allowed' }],
      betas: BETAS,
    });

    // The worker resumes and drives the run to a terminal end_turn.
    const denyReceiptId = denyReceipt.data[0]?.id;
    assert.equal(typeof denyReceiptId, 'string', 'D2 exact deny receipt');
    const { events: ended } = await waitForSessionEventReceipt(
      client,
      s.id,
      denyReceiptId,
      BETAS,
      ({ delta }) => [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn',
      'D2 durable Worker to commit the denied terminal Run',
    );
    pass('durable ingress: worker resumed the awaiting run after DENY and reached end_turn');

    // The effect is refused: the denied write has one error result and the
    // inherited-allow read is actually executed, but its linked result does not
    // contain the marker. Requiring the read/result pair prevents a vacuous
    // pass where no observation of the filesystem occurred.
    const results = ended.filter((e) => e.type === 'agent.tool_result');
    const deniedWrite = results.filter((result) => result.tool_use_id === toolUse.id);
    assert.equal(deniedWrite.length, 1, 'DENY records one result for the write occurrence');
    assert.equal(deniedWrite[0].is_error, true, 'the denied write is a model-visible error');
    const readUses = ended.filter((event) => event.type === 'agent.tool_use' && event.name === 'read');
    assert.equal(readUses.length, 1, 'the inherited-allow read executes exactly once after denial');
    const readResults = results.filter((result) => result.tool_use_id === readUses[0].id);
    assert.equal(readResults.length, 1, 'the read has one linked durable result');
    assert.ok(
      !JSON.stringify(readResults[0].content).includes(MARK),
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

    console.log('E2E PASS: durable HITL DENY — worker resumes, refuses the effect, ends the run, exactly-once.');
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
