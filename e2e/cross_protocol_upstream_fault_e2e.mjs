// Cross-protocol upstream-fault e2e (scenarios #90/#91 on non-managed wires): the
// runtime's retry / backoff / circuit-breaker + terminal-fault handling is protocol
// neutral, but the existing fault tests only drove the MANAGED wire. This drives the
// same fault paths through the AI-SDK, AG-UI, and Managed adapters:
//   • a retryable upstream failure is retried and the turn RECOVERS (transparent) on
//     both AI-SDK and AG-UI;
//   • a persistent failure EXHAUSTS retries and never fabricates a reply, and now
//     SURFACES as the wire's terminal error frame (AI-SDK `error` + `finish("error")`,
//     AG-UI `RUN_ERROR`, A2A a `failed` Task carrying the fault message) — not a
//     silent empty finish / falsely `completed` task;
//   • the SAME fault surfaces on the Managed wire as `session.error`.
//
// This exercises the fix that carries a terminal `EndCause::Error` through the
// `StepOutcome.failure` field (the neutral twin of Managed's `TurnFailure`), which
// each adapter renders via its `AgentEvent::RunFailed` transcoder mapping. Before the
// fix, the ProtocolRuntime twin (`to_step_outcome`) dropped the fault and the three
// non-managed wires closed the stream cleanly with no content.
//
// Hermetic (fake upstream, no live key). Run: (from e2e/) node cross_protocol_upstream_fault_e2e.mjs

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];

const FAKE_KEY = 'sk-fake-xproto-fault'; // awaken-allow: secret
const BASE_PORT = Number(process.env.E2E_PORT ?? 38613);

// Parse an SSE body into decoded data frames.
function frames(raw) {
  const out = [];
  for (const line of raw.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('data:')) continue;
    const p = t.slice(5).trim();
    if (!p || p === '[DONE]') continue;
    try {
      out.push(JSON.parse(p));
    } catch {
      /* ignore */
    }
  }
  return out;
}

async function aiSdkTurn(base, thread, text) {
  const res = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ threadId: thread, messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text }] }] }),
  });
  const evs = frames(await res.text());
  const reply = evs.filter((e) => e.type === 'text-delta').map((e) => e.delta).join('');
  return { status: res.status, evs, reply };
}

async function agUiTurn(base, thread, text) {
  const res = await fetch(`${base}/v1/ag-ui/agents/assistant`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      threadId: thread,
      runId: `run-${randomBytes(3).toString('hex')}`,
      messages: [{ id: 'u1', role: 'user', content: text }],
      tools: [],
      context: [],
      state: {},
      forwardedProps: {},
    }),
  });
  const evs = frames(await res.text());
  const reply = evs.filter((e) => e.type === 'TEXT_MESSAGE_CONTENT').map((e) => e.delta).join('');
  return { status: res.status, evs, reply };
}

// A2A message/send (JSON-RPC): returns the whole Task, so a terminal fault reads
// back as `Task.status.state === 'failed'` carrying the fault message.
async function a2aSend(base, context, text) {
  const res = await fetch(`${base}/v1/a2a`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 1,
      method: 'message/send',
      params: {
        message: {
          messageId: `m-${randomBytes(4).toString('hex')}`,
          contextId: context,
          role: 'user',
          kind: 'message',
          parts: [{ kind: 'text', text }],
        },
      },
    }),
  });
  const body = await res.json();
  const task = body.result;
  const statusText = (task?.status?.message?.parts ?? [])
    .filter((p) => p.kind === 'text')
    .map((p) => p.text)
    .join('');
  return { status: res.status, error: body.error, state: task?.status?.state, statusText };
}

// Run `fn(base)` against a `real`-mode server pointed at a fault-injected upstream.
async function withFaultyUpstream(faultOpts, port, fn) {
  const upstream = await startFakeAnthropic(FAKE_KEY, faultOpts);
  process.env.ANTHROPIC_API_KEY = FAKE_KEY;
  process.env.ANTHROPIC_MODEL = 'fake-haiku';
  process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
  try {
    return await withServer('real', port, (base) => fn(base, upstream));
  } finally {
    upstream.close();
  }
}

async function main() {
  const TEXT = `hi-${randomBytes(3).toString('hex')}`;

  // Arm 1: AI-SDK — a retryable failure recovers.
  await withFaultyUpstream({ failuresBeforeSuccess: 1 }, BASE_PORT, async (base, upstream) => {
    const { status, reply } = await aiSdkTurn(base, `xf-ai-ok-${randomBytes(2).toString('hex')}`, TEXT);
    assert.equal(status, 200, 'ai-sdk run accepted');
    assert.ok(reply.includes(`FAKE:${TEXT}`), `AI-SDK turn recovered on retry: ${JSON.stringify(reply)}`);
    assert.ok(upstream.attempts >= 2, `the failed attempt was retried (attempts=${upstream.attempts})`);
  });
  pass('AI-SDK: a retryable upstream failure is retried and the turn recovers');

  // Arm 2: AG-UI — a retryable failure recovers.
  await withFaultyUpstream({ failuresBeforeSuccess: 1 }, BASE_PORT + 1, async (base, upstream) => {
    const { status, reply } = await agUiTurn(base, `xf-ag-ok-${randomBytes(2).toString('hex')}`, TEXT);
    assert.equal(status, 200, 'ag-ui run accepted');
    assert.ok(reply.includes(`FAKE:${TEXT}`), `AG-UI turn recovered on retry: ${JSON.stringify(reply)}`);
    assert.ok(upstream.attempts >= 2, `the failed attempt was retried (attempts=${upstream.attempts})`);
  });
  pass('AG-UI: a retryable upstream failure is retried and the turn recovers');

  // Arm 3: AI-SDK — a persistent failure exhausts and SURFACES as an error frame.
  await withFaultyUpstream({ alwaysFail: true }, BASE_PORT + 2, async (base, upstream) => {
    const { status, evs, reply } = await aiSdkTurn(base, `xf-ai-err-${randomBytes(2).toString('hex')}`, TEXT);
    assert.equal(status, 200, 'ai-sdk stream opened (the error rides in-band)');
    assert.ok(!reply.includes('FAKE:'), 'a permanently-failing upstream does not fabricate a reply');
    assert.ok(evs.some((e) => e.type === 'error'), `AI-SDK surfaces an error frame: ${JSON.stringify(evs.map((e) => e.type))}`);
    assert.ok(
      evs.some((e) => e.type === 'finish' && e.finishReason === 'error'),
      `AI-SDK closes with finish("error"): ${JSON.stringify(evs.filter((e) => e.type === 'finish'))}`,
    );
    assert.ok(upstream.attempts >= 2, `retries were attempted before giving up (attempts=${upstream.attempts})`);
  });
  pass('AI-SDK: exhausted upstream -> error frame + finish("error"), no fabricated reply');

  // Arm 4: AG-UI — a persistent failure exhausts and SURFACES as RUN_ERROR.
  await withFaultyUpstream({ alwaysFail: true }, BASE_PORT + 3, async (base, upstream) => {
    const { status, evs, reply } = await agUiTurn(base, `xf-ag-err-${randomBytes(2).toString('hex')}`, TEXT);
    assert.equal(status, 200, 'ag-ui stream opened');
    assert.ok(!reply.includes('FAKE:'), 'AG-UI does not fabricate a reply on persistent failure');
    assert.ok(upstream.attempts >= 2, `retries were attempted (attempts=${upstream.attempts})`);
    assert.ok(evs.some((e) => e.type === 'RUN_ERROR'), `AG-UI surfaces RUN_ERROR: ${JSON.stringify(evs.map((e) => e.type))}`);
    assert.ok(!evs.some((e) => e.type === 'TEXT_MESSAGE_CONTENT'), 'AG-UI emitted no content on a failed run');
  });
  pass('AG-UI: exhausted upstream -> RUN_ERROR, no fabricated reply');

  // Arm 5: the SAME fault on the MANAGED wire DOES surface — `session.error`. This
  // proves the failure is real and pins the cross-protocol asymmetry.
  await withFaultyUpstream({ alwaysFail: true }, BASE_PORT + 4, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    let sendError = null;
    try {
      await client.beta.sessions.events.send(session.id, {
        betas: BETAS,
        events: [{ type: 'user.message', content: [{ type: 'text', text: TEXT }] }],
      });
    } catch (err) {
      sendError = err;
    }
    const events = [];
    try {
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
    } catch {
      /* list may also reject on a hard failure */
    }
    const surfaced = sendError !== null || events.some((e) => e.type === 'session.error' || (e.type ?? '').includes('error'));
    assert.ok(surfaced, `Managed surfaces the terminal fault (session.error / send error): ${JSON.stringify(events.map((e) => e.type))}`);
    const fabricated = events.filter((e) => e.type === 'agent.message').some((e) => (e.content ?? []).some((b) => (b.text ?? '').includes('FAKE:')));
    assert.ok(!fabricated, 'Managed does not fabricate a reply either');
  });
  pass('Managed: the SAME fault DOES surface (session.error / send error) — the asymmetry vs AI-SDK/AG-UI');

  // Arm 6: A2A — a persistent failure reads back as a `failed` Task (not a falsely
  // `completed` one), carrying the fault message. Closes the wire-symmetry gap.
  await withFaultyUpstream({ alwaysFail: true }, BASE_PORT + 5, async (base, upstream) => {
    const r = await a2aSend(base, `xf-a2a-err-${randomBytes(2).toString('hex')}`, TEXT);
    assert.equal(r.status, 200, 'a2a transport ok (the fault rides in the Task, not the transport)');
    assert.ok(!r.error, `a2a message/send is not a JSON-RPC error: ${JSON.stringify(r.error)}`);
    assert.equal(r.state, 'failed', `A2A terminal fault -> Task.state=failed (got ${r.state})`);
    // The failed Task carries the fault message (my fix routes `Terminal::Failed`'s
    // message into the Task status) rather than a fabricated agent reply.
    assert.ok(r.statusText.length > 0, `A2A failed Task carries the fault message (got ${JSON.stringify(r.statusText)})`);
    assert.ok(!r.statusText.includes('FAKE:'), 'the status is the fault, not a fabricated model reply');
    assert.ok(upstream.attempts >= 2, `retries were attempted (attempts=${upstream.attempts})`);
  });
  pass('A2A: exhausted upstream -> failed Task (not falsely completed), no fabricated reply');

  console.log('E2E PASS: cross-protocol upstream-fault handling (retry-recover on AI-SDK/AG-UI; terminal fault surfaces on all four wires).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
