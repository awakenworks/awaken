// All-in-one frontdoors e2e: proves that ONE `awaken-server-local` process exposes
// FOUR wire protocols at once. The harness boots a single server (`withRealServer`,
// so every door's turn runs through the real provider path against a fake Anthropic
// upstream reproducing the `echo` scenario), then we smoke each frontdoor against
// that SAME base URL with its official client SDK and assert a coherent reply:
//
//   - managed : @anthropic-ai/sdk  beta.sessions.* over /v1/sessions
//   - ai-sdk  : POST /v1/ai-sdk/chat, read the SSE UI message stream to [DONE]
//   - ag-ui   : @ag-ui/client HttpAgent at /v1/ag-ui/agents/assistant
//   - a2a     : @a2a-js/sdk A2AClient.fromCardUrl(/v1/a2a/agent-card) + message:send
//
// The point is breadth, not depth: each door must RESPOND from the one process.
// Run: (from e2e/)  node allinone_frontdoors_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { HttpAgent } from '@ag-ui/client';
import { A2AClient } from '@a2a-js/sdk/client';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38423);
const BETAS = ['managed-agents-2026-04-01'];

// --- managed (Anthropic TS SDK, /v1/sessions) --------------------------------
// Create a session, send a user turn, list the events, and read the assistant
// reply the real provider wire produced (echo scenario -> "Echo: <text>").
async function checkManaged(base) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  assert.equal(session.type, 'session', 'managed session created');
  assert.ok(session.id.startsWith('sesn_'), 'managed session id looks right');

  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi there' }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
    events.push(ev);
  }
  const message = events.find((e) => e.type === 'agent.message');
  assert.ok(message, `managed door produced an agent.message: ${events.map((e) => e.type)}`);
  const text = message.content?.[0]?.text ?? '';
  assert.ok(text.includes('hi there'), `managed reply echoes the turn: ${text}`);
  pass(`managed  : /v1/sessions -> agent.message "${text}"`);
}

// --- ai-sdk (raw SSE, /v1/ai-sdk/chat) ---------------------------------------
// POST a UI message and read the UI Message Stream to its `[DONE]` sentinel,
// concatenating the `text-delta` frames into the assistant reply text.
async function checkAiSdk(base) {
  const res = await fetch(`${base}/v1/ai-sdk/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      threadId: 'allinone-sdk',
      messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: 'ai-sdk hello' }] }],
    }),
  });
  assert.equal(res.status, 200, 'ai-sdk stream accepted');
  const raw = await res.text();
  const frames = raw
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.startsWith('data: '))
    .map((l) => l.slice('data: '.length));
  assert.ok(frames.includes('[DONE]'), 'ai-sdk UI message stream closed with [DONE]');
  const text = frames
    .filter((d) => d !== '[DONE]')
    .map((d) => JSON.parse(d))
    .filter((f) => f.type === 'text-delta')
    .map((f) => f.delta)
    .join('');
  assert.ok(text.length > 0, 'ai-sdk assistant text is non-empty');
  assert.ok(text.includes('ai-sdk hello'), `ai-sdk reply echoes the turn: ${text}`);
  pass(`ai-sdk   : POST /v1/ai-sdk/chat -> SSE [DONE], text "${text}"`);
}

// --- ag-ui (@ag-ui/client HttpAgent) -----------------------------------------
// Run the agent and read the assistant reply from the run's newMessages.
async function checkAgUi(base) {
  const agent = new HttpAgent({ url: `${base}/v1/ag-ui/agents/assistant` });
  agent.messages = [{ id: 'u1', role: 'user', content: 'ag-ui hello' }];
  const run = await agent.runAgent();
  const produced = run?.newMessages ?? [];
  const last = produced[produced.length - 1];
  assert.ok(last && last.role === 'assistant', `ag-ui returned an assistant message: ${last?.role}`);
  const text =
    typeof last.content === 'string'
      ? last.content
      : (last.content ?? []).map((c) => c.text ?? '').join('');
  assert.ok(text.includes('ag-ui hello'), `ag-ui reply echoes the turn: ${text}`);
  pass(`ag-ui    : /v1/ag-ui/agents/assistant -> assistant "${text}"`);
}

// --- a2a (@a2a-js/sdk A2AClient) ---------------------------------------------
// Resolve the client from the agent card, message:send, read the Task's reply.
async function checkA2a(base) {
  const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
  const res = await client.sendMessage({
    message: {
      messageId: 'm1',
      contextId: 'allinone-a2a',
      role: 'user',
      kind: 'message',
      parts: [{ kind: 'text', text: 'a2a hello' }],
    },
  });
  assert.ok(res.result, 'a2a message:send returned a JSON-RPC result');
  const text = (res.result?.status?.message?.parts ?? [])
    .filter((p) => p.kind === 'text')
    .map((p) => p.text)
    .join('');
  assert.ok(text.includes('a2a hello'), `a2a reply echoes the turn: ${text}`);
  pass(`a2a      : /v1/a2a/agent-card + message:send -> "${text}"`);
}

async function main() {
  await withRealServer('echo', PORT, async (base) => {
    console.log(`one awaken-server-local process at ${base} — smoking four frontdoors:`);
    await checkManaged(base);
    await checkAiSdk(base);
    await checkAgUi(base);
    await checkA2a(base);
  });
  pass('all-in-one frontdoors smoke');
  console.log(
    'E2E PASS: one awaken-server-local process serves managed + ai-sdk + ag-ui + a2a frontdoors.',
  );
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
