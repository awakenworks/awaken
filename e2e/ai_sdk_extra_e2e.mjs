// AI SDK adapter — the endpoints and paths the `useChat` happy-path e2e does not
// reach: the agent-scoped run route, the plain `/chat` route, the thread history
// GET, and the malformed-body error path. Driven with raw HTTP against the AI SDK
// v6 UI Message Stream wire shape (the SSE stream is consumed as text).
//
// Run: (from e2e/)  node ai_sdk_extra_e2e.mjs

import assert from 'node:assert/strict';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv, streamedText } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38186);
const BASE = `http://127.0.0.1:${PORT}`;

let seq = 0;
const body = (text, threadId) =>
  JSON.stringify({
    messages: [{ id: `m-${seq++}`, role: 'user', parts: [{ type: 'text', text }] }],
    threadId,
  });

const postStream = async (path, payload) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: payload,
  });
  return { status: res.status, text: await res.text() };
};

async function main() {
  const upstream = await startUpstream('echo');
  const { server } = spawnServer('real', PORT, { ...realServerEnv('echo', upstream) });
  await waitForPort(PORT);
  try {
    // Agent-scoped run route: `/v1/ai-sdk/agents/:agent_id/runs` sets the agent id
    // from the path; the reply streams back as text-delta events.
    const scoped = await postStream('/v1/ai-sdk/agents/assistant/runs', body('AGENT-SCOPED', 'aiextra'));
    assert.equal(scoped.status, 200, 'agent-scoped run accepted');
    assert.ok(streamedText(scoped.text).includes('Echo: AGENT-SCOPED'), 'agent-scoped run streamed the reply');
    pass('agent-scoped run route streamed a reply (chat_agent_scoped)');

    // Thread history GET: projects committed truth to the AI SDK history shape.
    const hist = await fetch(`${BASE}/v1/ai-sdk/threads/aiextra/messages`);
    assert.equal(hist.status, 200, 'history fetched');
    const histText = JSON.stringify(await hist.json());
    assert.ok(histText.includes('AGENT-SCOPED'), 'history includes the prior user turn');
    assert.ok(histText.includes('Echo: AGENT-SCOPED'), 'history includes the assistant reply');
    pass('thread history GET projected committed truth (thread_messages / HistoryResponse)');

    // Cursor pagination over the same persisted thread: ?size + ?cursor walk it.
    // House cursor-page envelope (awaken-api-contract): { items, cursor }.
    const hp1 = await (await fetch(`${BASE}/v1/ai-sdk/threads/aiextra/messages?size=1`)).json();
    assert.equal(hp1.items.length, 1, 'history page 1 holds one message');
    assert.equal(typeof hp1.cursor, 'string', 'history page 1 carries a continuation cursor');
    const hp2 = await (await fetch(
      `${BASE}/v1/ai-sdk/threads/aiextra/messages?size=1&cursor=${encodeURIComponent(hp1.cursor)}`,
    )).json();
    assert.ok(hp2.items.length >= 1, 'history page 2 resumes after the cursor');
    assert.notEqual(hp2.items[0].id, hp1.items[0].id, 'history page 2 does not repeat page 1');
    const hBad = await fetch(`${BASE}/v1/ai-sdk/threads/aiextra/messages?cursor=this-id-does-not-exist`);
    assert.equal(hBad.status, 400, 'a fabricated cursor is a 400');
    pass('history is paged by cursor (?size + ?cursor, items/cursor, bad cursor → 400)');

    // Plain `/chat` route (no thread/agent in the path).
    const plain = await postStream('/v1/ai-sdk/chat', body('PLAIN-CHAT', 'aichat'));
    assert.equal(plain.status, 200, 'plain chat accepted');
    assert.ok(streamedText(plain.text).includes('Echo: PLAIN-CHAT'), 'plain chat streamed the reply');
    pass('plain /chat route streamed a reply (chat)');

    // Malformed body → the JSON extractor fails closed with an error stream, not a
    // panic; the UI stream carries an error event.
    const bad = await fetch(`${BASE}/v1/ai-sdk/chat`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: '{ this is not valid json',
    });
    const badText = await bad.text();
    assert.ok(badText.toLowerCase().includes('error'), 'a malformed body yields an error stream, not a crash');
    pass('malformed body failed closed with an error event (AiSdkJson decode path)');

    console.log('AI SDK EXTRA PASS: agent-scoped + history + plain chat + error paths.');
    console.log('E2E PASS: ai-sdk extra endpoints and error paths.');
  } finally {
    await stopServer(server);
    upstream.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
