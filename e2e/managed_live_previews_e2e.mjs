// Managed Agents live previews (SDK-compatible), driving the REAL provider path
// (GenaiExecutor streaming) against the fake Anthropic upstream, which chunks the
// assistant text across several `text_delta` events exactly as Anthropic streams.
//
// A client opts in with `event_deltas[]=agent.message` on the session event
// stream. The server then previews the in-flight `agent.message` text as the
// official wire does:
//
//   {"type":"event_start","event":{"type":"agent.message","id":"evt_.."}}
//   {"type":"event_delta","event_id":"evt_..","delta":{"type":"content_delta",
//        "index":0,"content":{"type":"text","text":".."}}}
//
// It asserts, over the SSE wire the official @anthropic-ai/sdk beta.agents client
// consumes:
//   - one event_start announcing the upcoming agent.message + its id,
//   - N content_delta frames whose text concatenates to the buffered message,
//   - the buffered agent.message carries the SAME id (so the SDK's preview
//     accumulator reconciles preview -> committed by id),
//   - without the opt-in, NO preview frames appear (committed events only),
//   - a bad event_deltas value is rejected 400,
//   - the per-thread stream rejects the opt-in 400 (session-level only).
//
// Run: (from e2e/)  node managed_live_previews_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38455);

const HEADERS = {
  'x-api-key': 'e2e-dummy',
  'anthropic-version': '2023-06-01',
  'anthropic-beta': 'managed-agents-2026-04-01',
};

// Open an SSE GET stream and read its `data:` frames to completion (the server
// ends the stream after the turn's terminal `session.status_idle`). A timeout
// aborts a stream that never terminates so the test fails loudly instead of hanging.
async function readSseToEnd(url) {
  const res = await fetch(url, { headers: HEADERS, signal: AbortSignal.timeout(20_000) });
  if (res.status !== 200) {
    return { status: res.status, frames: [] };
  }
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  const frames = [];
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let sep;
    while ((sep = buf.indexOf('\n\n')) !== -1) {
      const block = buf.slice(0, sep);
      buf = buf.slice(sep + 2);
      for (const line of block.split('\n')) {
        const t = line.trim();
        if (t.startsWith('data: ')) frames.push(JSON.parse(t.slice('data: '.length)));
      }
    }
  }
  return { status: res.status, frames };
}

// Open the stream first, then send concurrently, so the still-open stream sees the
// turn's in-flight previews (the run commits synchronously inside `events.send`).
async function streamTurn(baseUrl, client, sessionId, text, query) {
  const url = `${baseUrl}/v1/sessions/${sessionId}/events/stream${query}`;
  // Subscribe (open the stream) before sending — the GET handler subscribes to the
  // session broadcast before its first byte, so previews can't slip the gap.
  const res = await fetch(url, { headers: HEADERS, signal: AbortSignal.timeout(20_000) });
  assert.equal(res.status, 200, 'session event stream accepted');
  const reader = res.body.getReader();
  const decoder = new TextDecoder();

  const send = client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });

  let buf = '';
  const frames = [];
  let idled = false;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let sep;
    while ((sep = buf.indexOf('\n\n')) !== -1) {
      const block = buf.slice(0, sep);
      buf = buf.slice(sep + 2);
      for (const line of block.split('\n')) {
        const t = line.trim();
        if (t.startsWith('data: ')) {
          const frame = JSON.parse(t.slice('data: '.length));
          frames.push(frame);
          if (frame.type === 'session.status_idle') idled = true;
        }
      }
    }
    if (idled) break;
  }
  await send;
  return frames;
}

async function main() {
  try {
    await withRealServer('echo', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // --- previews opted in: event_start + N content_delta reconcile to the buffered message ---
      {
        const session = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: 'env_local',
          betas: BETAS,
        });
        const frames = await streamTurn(
          baseUrl,
          client,
          session.id,
          'stream-these-previews',
          '?event_deltas[]=agent.message',
        );

        const start = frames.find((f) => f.type === 'event_start');
        assert.ok(start, 'server emitted an event_start');
        assert.equal(start.event.type, 'agent.message', 'event_start announces an agent.message');
        const previewId = start.event.id;
        assert.ok(previewId, 'event_start carries the upcoming message id');

        const deltas = frames.filter(
          (f) => f.type === 'event_delta' && f.event_id === previewId,
        );
        assert.ok(deltas.length >= 2, `text streamed incrementally (got ${deltas.length} content_delta frames)`);
        for (const d of deltas) {
          assert.equal(d.delta.type, 'content_delta', 'delta is a content_delta (not content_block_delta)');
          assert.equal(d.delta.index, 0, 'content_delta index');
          assert.equal(d.delta.content.type, 'text', 'content_delta carries text');
        }
        const streamedText = deltas.map((d) => d.delta.content.text).join('');

        // The buffered agent.message: authoritative, and it carries the SAME id so
        // the SDK's live-preview accumulator discards the preview and renders it.
        const committed = frames.find((f) => f.type === 'agent.message');
        assert.ok(committed, 'the buffered agent.message committed');
        assert.equal(committed.id, previewId, 'buffered agent.message reuses the preview id (reconcile by id)');
        const committedText = committed.content.map((b) => b.text ?? '').join('');
        assert.equal(streamedText, committedText, 'concatenated content_delta text equals the buffered message');

        // event_start / event_delta are stream-only — never in the committed log.
        const listed = [];
        for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) listed.push(ev);
        assert.ok(
          listed.every((e) => e.type !== 'event_start' && e.type !== 'event_delta'),
          'preview frames are never persisted in events.list()',
        );
        pass(`live previews: event_start + ${deltas.length} content_delta reconcile to the buffered agent.message by id`);
      }

      // --- no opt-in: the same turn streams committed events only, zero previews ---
      {
        const session = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: 'env_local',
          betas: BETAS,
        });
        const frames = await streamTurn(baseUrl, client, session.id, 'no-previews-here', '');
        assert.ok(
          frames.some((f) => f.type === 'agent.message'),
          'the turn still delivered the committed agent.message',
        );
        assert.ok(
          !frames.some((f) => f.type === 'event_start' || f.type === 'event_delta'),
          'no preview frames without the event_deltas[] opt-in',
        );
        pass('without event_deltas[], the stream carries committed events only (no previews)');
      }

      // --- validation: an unsupported event_deltas value is rejected 400 ---
      {
        const session = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: 'env_local',
          betas: BETAS,
        });
        const { status } = await readSseToEnd(
          `${baseUrl}/v1/sessions/${session.id}/events/stream?event_deltas[]=agent.tool_use`,
        );
        assert.equal(status, 400, 'an unsupported event_deltas value is a 400');

        // The per-thread stream rejects the opt-in (session-level only). The primary
        // thread id mirrors the session id in this server.
        const thread = `${baseUrl}/v1/sessions/${session.id}/threads/${session.id}/stream?event_deltas[]=agent.message`;
        const threadRes = await readSseToEnd(thread);
        assert.equal(threadRes.status, 400, 'the per-thread stream rejects event_deltas[] with 400');
        pass('event_deltas[] validation: bad value 400, and rejected on the per-thread stream');
      }
    });

    console.log('\nE2E PASS: Managed Agents live previews (event_start / content_delta) are SDK-compatible.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
