// Managed resilience / fault-injection e2e (ported from awaken-next resilience
// cases, restricted to our SDK surface). Drives the managed sessions API through
// the official @anthropic-ai/sdk. Ported scenarios:
//   - user.interrupt with no active run          -> accepted, session stays usable
//   - concurrent sends to ONE session            -> all settle, server stays responsive
//   - server recovers after a malformed request  -> 4xx, then a valid call succeeds
//   - duplicate tool_confirmation of a resolved tool -> fail closed (no double-run)
//
// Deterministic: echo mode (turns complete) for the first four, probe mode (a turn
// awaits on a tool) for the duplicate-confirmation case.
//
// Run: (from e2e/)  node managed_resilience_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

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

async function listEvents(c, id) {
  const events = [];
  for await (const ev of c.beta.sessions.events.list(id, { betas: BETAS })) events.push(ev);
  return events;
}

const newSession = (c) =>
  c.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });

async function resilientPaths(echoUp) {
  const a = spawnServer('real', PORT, { ...realServerEnv('echo', echoUp) });
  try {
    await waitForPort(PORT);
    const c = client(a.baseUrl);

    // 1. user.interrupt with no active run is accepted (idempotent-noop), and the
    //    session remains usable for a subsequent turn.
    {
      const s = await newSession(c);
      await c.beta.sessions.events.send(s.id, {
        events: [{ type: 'user.interrupt' }],
        betas: BETAS,
      });
      await c.beta.sessions.events.send(s.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'after-interrupt' }] }],
        betas: BETAS,
      });
      const idle = [...(await listEvents(c, s.id))].reverse().find((e) => e.type === 'session.status_idle');
      assert.equal(idle.stop_reason.type, 'end_turn', 'session usable after a no-op interrupt');
      pass('user.interrupt with no active run -> accepted, session still usable');
    }


    // 3. Concurrent sends to ONE session: the host serializes per thread; every
    //    request settles and the server stays responsive afterward (no crash/hang).
    {
      const s = await newSession(c);
      const sends = Array.from({ length: 6 }, (_, i) =>
        c.beta.sessions.events.send(s.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: `concurrent-${i}` }] }],
          betas: BETAS,
        }),
      );
      const settled = await Promise.allSettled(sends);
      assert.ok(
        settled.some((r) => r.status === 'fulfilled'),
        'at least one concurrent send succeeded',
      );
      // The server is still alive: a fresh session round-trips.
      const probe = await newSession(c);
      assert.ok(probe.id, 'server stayed responsive after concurrent load');
      pass(`concurrent sends to one session -> all settled, server responsive`);
    }

    // 4. A malformed request is rejected, and the server recovers (next call works).
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
    const s = await newSession(c);
    await c.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'DUP-CONFIRM' }] }],
      betas: BETAS,
    });
    const awaiting = (await listEvents(c, s.id)).find((e) => e.type === 'agent.tool_use');
    assert.ok(awaiting, 'run awaiting on a tool_use');

    // First confirmation resolves the tool and completes the turn.
    await c.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: awaiting.id, result: 'allow' }],
      betas: BETAS,
    });
    const idle = [...(await listEvents(c, s.id))].reverse().find((e) => e.type === 'session.status_idle');
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
