// Managed resilience / fault-injection e2e (ported from awaken-next resilience
// cases, restricted to our SDK surface). Drives the managed sessions API through
// the official @anthropic-ai/sdk. Ported scenarios:
//   - user.interrupt with no active run          -> accepted, session stays usable
//   - server recovers after a malformed request  -> 4xx, then a valid call succeeds
//   - duplicate tool_confirmation of a resolved tool -> fail closed (no double-run)
//
// Deterministic: echo mode (turns complete) for the first two, probe mode (a turn
// awaits on a tool) for the duplicate-confirmation case.
//
// Run: (from e2e/)  node managed_resilience_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  managedAgentWithAlwaysAskTools,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  pass,
  startUpstream,
  realServerEnv,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38232);

const client = (base) => new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

async function statusOf(promise, what) {
  try {
    await promise;
  } catch (err) {
    if (typeof err?.status === 'number') return err.status;
    throw new Error(`${what}: threw a non-API error: ${err}`);
  }
  throw new Error(`${what}: expected an error but the call succeeded`);
}

const isClientError = (s) => s >= 400 && s < 500;

const newSession = (c, agent = 'assistant') =>
  c.beta.sessions.create({ agent, environment_id: 'env_local', betas: BETAS });

async function resilientPaths(echoUp) {
  const a = spawnServer('real', PORT, { ...realServerEnv('echo', echoUp) });
  try {
    await waitForPort(PORT);
    const c = client(a.baseUrl);

    // 1. user.interrupt with no active run is accepted (idempotent-noop), and the
    //    session remains usable for a subsequent turn.
    {
      const s = await newSession(c);
      // R1: C1=no-op interrupt receipt commits; C2=later User receipt reaches
      // end_turn. E1=Session remains reusable. Constraint: C2 follows committed
      // C1 and old idle cannot qualify. C1&&!C2=>observe; C1+C2=>E1.
      const interrupt = await c.beta.sessions.events.send(s.id, {
        events: [{ type: 'user.interrupt' }],
        betas: BETAS,
      });
      await waitForSessionEventReceipt(
        c,
        s.id,
        interrupt.data[0]?.id,
        BETAS,
        () => true,
        'R1 no-op interrupt receipt commits',
      );
      const message = await c.beta.sessions.events.send(s.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'after-interrupt' }] }],
        betas: BETAS,
      });
      const { delta } = await waitForSessionEventReceipt(
        c,
        s.id,
        message.data[0]?.id,
        BETAS,
        ({ delta: current }) => current.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
        'R1 post-interrupt turn reaches end_turn',
      );
      const idle = [...delta].reverse().find((event) => event.type === 'session.status_idle');
      assert.equal(idle.stop_reason.type, 'end_turn', 'session usable after a no-op interrupt');
      pass('user.interrupt with no active run -> accepted, session still usable');
    }


    // 2. A malformed request is rejected, and the server recovers (next call works).
    {
      const bad = await fetch(`${a.baseUrl}/v1/sessions`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
        body: '{ this is not valid json',
      });
      assert.ok(isClientError(bad.status), `malformed body -> client error (got ${bad.status})`);
      const ok = await newSession(c);
      assert.ok(ok.id, 'server recovered after a malformed request');
      pass(`malformed request -> ${bad.status}, server recovered`);
    }
  } finally {
    await stopServer(a.server);
  }
}

async function duplicateConfirmation(probeUp) {
  const a = spawnServer('real', PORT + 1, { ...realServerEnv('probe', probeUp) });
  try {
    await waitForPort(PORT + 1);
    const c = client(a.baseUrl);
    const s = await newSession(c, managedAgentWithAlwaysAskTools(['write']));
    // R2: C0=the Session owns write=always_ask; C1=exact task receipt reaches
    // requires_action; C2=exact allow receipt
    // reaches end_turn. E1=duplicate C2 is then rejected. Constraint: each phase
    // is scoped after its own receipt. C1&&!C2=>awaiting; C1+C2=>E1.
    const task = await c.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'DUP-CONFIRM' }] }],
      betas: BETAS,
    });
    const { delta: awaitingEvents } = await waitForSessionEventReceipt(
      c,
      s.id,
      task.data[0]?.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.tool_use')
        && delta.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'requires_action'),
      'R2 task reaches requires_action',
    );
    const awaiting = awaitingEvents.find((event) => event.type === 'agent.tool_use');
    assert.ok(awaiting, 'run awaiting on a tool_use');

    // First confirmation resolves the tool and completes the turn.
    const allow = await c.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: awaiting.id, result: 'allow' }],
      betas: BETAS,
    });
    const { delta: completed } = await waitForSessionEventReceipt(
      c,
      s.id,
      allow.data[0]?.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle'
        && event.stop_reason?.type === 'end_turn'),
      'R2 allow reaches end_turn',
    );
    const idle = [...completed].reverse().find((event) => event.type === 'session.status_idle');
    assert.equal(idle.stop_reason.type, 'end_turn', 'first confirmation completed the turn');

    // Re-confirming the SAME (now-resolved) tool_use_id must fail closed — no
    // double-run of the tool's side effect.
    const status = await statusOf(
      c.beta.sessions.events.send(s.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: awaiting.id, result: 'allow' }],
        betas: BETAS,
      }),
      're-confirm resolved tool',
    );
    assert.ok(isClientError(status), `re-confirming a resolved tool fails closed (got ${status})`);
    pass(`duplicate tool_confirmation of a resolved tool -> fail closed (${status})`);
  } finally {
    await stopServer(a.server);
  }
}

async function main() {
  const echoUp = await startUpstream('echo');
  const probeUp = await startUpstream('probe');
  try {
    await resilientPaths(echoUp);
    await duplicateConfirmation(probeUp);
    console.log('E2E PASS: managed resilience / fault paths behave correctly.');
  } finally {
    echoUp.close();
    probeUp.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
