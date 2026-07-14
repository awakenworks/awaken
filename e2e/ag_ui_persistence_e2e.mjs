// AG-UI server-side message persistence + cursor pagination. AG-UI is a
// client-forward protocol (the HttpAgent replays history in each RunAgentInput),
// but the server also persists every committed turn, so a client that lost its
// state rehydrates from GET /v1/ag-ui/threads/{id}/messages. This proves:
//   1. Persistence: two runs that each send ONLY the new user message (no replayed
//      history) still accumulate the full transcript server-side.
//   2. Shape: history projects to the AG-UI message shape ({ id, role, content }).
//   3. Cursor pagination in the house awaken-api-contract shape: ?size + ?cursor
//      walk the thread; the response is a CursorPage `{ items, cursor }`.
//   4. Edge arms: unknown thread → empty page; fabricated cursor → 400.
//
// Mirrors ~/Codes/awaken-next e2e/coverage/scenario-ag-ui.mjs (the messages +
// ?limit history surface).
//
// Run: (from e2e/)  node ag_ui_persistence_e2e.mjs

import assert from 'node:assert/strict';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv, streamedText } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38193);
const BASE = `http://127.0.0.1:${PORT}`;
const TID = `agui-persist-${Date.now()}`;

// Drive one AG-UI turn on TID, sending only the new user message (no replayed
// history), and drain the SSE stream to force the commit before we read back.
async function runTurn(id, content) {
  const res = await fetch(`${BASE}/v1/ag-ui`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ threadId: TID, messages: [{ id, role: 'user', content }] }),
  });
  assert.equal(res.status, 200, `run "${content}" accepted`);
  const text = await res.text();
  assert.ok(streamedText(text).includes(`Echo: ${content}`), `run "${content}" streamed the reply`);
}

async function messages(query = '') {
  const res = await fetch(`${BASE}/v1/ag-ui/threads/${TID}/messages${query}`);
  return res;
}

async function main() {
  const upstream = await startUpstream('echo');
  const { server } = spawnServer('real', PORT, { ...realServerEnv('echo', upstream) });
  await waitForPort(PORT);
  try {
    // Two separate turns, each sending only its own new user message.
    await runTurn('q1', 'FIRST');
    await runTurn('q2', 'SECOND');
    pass('two turns committed without the client replaying prior history');

    // Full history: server persisted BOTH turns even though neither request
    // carried the other's messages — this is the server-side persistence proof.
    const fullRes = await messages();
    assert.equal(fullRes.status, 200, 'history fetched');
    const full = await fullRes.json();
    // House cursor-page envelope (awaken-api-contract): { items, cursor }.
    assert.ok(Array.isArray(full.items), 'history has an items array');
    // CursorPage omits `cursor` on the last page (skip_serializing_if=None).
    assert.ok(full.cursor == null, 'full page carries no continuation cursor');
    const blob = JSON.stringify(full.items);
    for (const needle of ['FIRST', 'Echo: FIRST', 'SECOND', 'Echo: SECOND']) {
      assert.ok(blob.includes(needle), `history includes "${needle}"`);
    }
    // Every message is in the AG-UI shape.
    for (const m of full.items) {
      assert.ok(typeof m.id === 'string' && m.id.length > 0, 'message has an id');
      assert.ok(['user', 'assistant', 'system', 'tool'].includes(m.role), `role ${m.role} is valid`);
      assert.ok('content' in m, 'message carries content');
    }
    const fullIds = full.items.map((m) => m.id);
    assert.ok(fullIds.length >= 4, 'at least the two user + two assistant turns persisted');
    pass(`server persisted the full ${fullIds.length}-message transcript across separate runs`);

    // Cursor pagination: first page of 1.
    const p1Res = await messages('?size=1');
    assert.equal(p1Res.status, 200, 'page 1 fetched');
    const p1 = await p1Res.json();
    assert.equal(p1.items.length, 1, 'page 1 holds exactly one message');
    assert.equal(p1.items[0].id, fullIds[0], 'page 1 starts at the oldest message');
    assert.equal(typeof p1.cursor, 'string', 'page 1 carries a continuation cursor (more remain)');
    assert.equal(p1.cursor, fullIds[0], 'the cursor names the last message of page 1');

    // Resume after the cursor: the rest of the thread, in order, no overlap.
    const p2Res = await messages(`?cursor=${encodeURIComponent(p1.cursor)}&size=50`);
    const p2 = await p2Res.json();
    assert.equal(p2.items[0].id, fullIds[1], 'page 2 resumes at the message after the cursor');
    const walked = [p1.items[0].id, ...p2.items.map((m) => m.id)];
    assert.deepEqual(walked, fullIds, 'the two pages reassemble the full transcript with no gap or overlap');
    pass('cursor pagination walks the persisted thread (?size + ?cursor, items/cursor)');

    // Unknown thread → an empty page, not an error.
    const unknownRes = await fetch(`${BASE}/v1/ag-ui/threads/does-not-exist-${Date.now()}/messages`);
    assert.equal(unknownRes.status, 200, 'unknown thread is a 200');
    const unknown = await unknownRes.json();
    assert.equal(unknown.items.length, 0, 'unknown thread has no messages');
    assert.ok(unknown.cursor == null, 'unknown thread carries no cursor');
    pass('unknown thread returns an empty page, not an error');

    // Fabricated cursor → a 400 caller error.
    const badRes = await messages('?cursor=this-id-does-not-exist');
    assert.equal(badRes.status, 400, 'a fabricated cursor is a 400');
    pass('a fabricated cursor fails closed with a 400');

    console.log('E2E PASS: ag-ui server-side persistence + cursor pagination.');
  } finally {
    await stopServer(server);
    upstream.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
