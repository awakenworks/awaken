// Cross-protocol verification: does each wire protocol expose live tool-input
// deltas before its authoritative completion event, or deliver a unary result?
//
// Drives the REAL provider path (GenaiExecutor streaming) against the fake
// Anthropic upstream, which chunks `input_json_delta` exactly as Anthropic does.
// The `use-tool:read` prompt makes the model emit a `read` tool call with input
// `{"pattern":"*.md"}`; each adapter is then checked over its own wire:
//
//   - ai-sdk : tool-input-start + live tool-input-delta + tool-input-available
//   - ag-ui  : TOOL_CALL_START + live TOOL_CALL_ARGS + TOOL_CALL_END
//   - a2a    : request/response (message:send returns one Task; streaming is
//              omitted from the A2A binding by design) — verified as NON-streaming
//
// Run: (from e2e/)  node streaming_tool_input_e2e.mjs

import assert from 'node:assert/strict';
import { A2AClient } from '@a2a-js/sdk/client';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38271);
const EXPECTED_ARGS = '{"pattern":"*.md"}';

// POST a body and read the SSE response as text, returning the decoded `data:`
// frames (skipping the AI SDK `[DONE]` sentinel).
async function postSse(url, body) {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  const frames = text
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.startsWith('data: '))
    .map((l) => l.slice('data: '.length))
    .filter((d) => d !== '[DONE]')
    .map((d) => JSON.parse(d));
  return { status: res.status, contentType: res.headers.get('content-type') ?? '', frames };
}

// Causes: A1 the fake provider emits multiple input_json_delta fragments; A2
// the shared live-delta buffer may coalesce adjacent short fragments; A3 the
// authoritative call closes. Effects: E1 at least one live input delta precedes
// availability, E2 all delta bytes concatenate to the exact input, E3 the
// available frame carries the parsed object. Rules A1+A2=>E1+E2 (one or more
// wire deltas), A3=>E3; frame count is not a provider-fragment count.
async function checkAiSdk(base) {
  const { status, frames } = await postSse(`${base}/v1/ai-sdk/chat`, {
    threadId: 'sdk-stream',
    messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: 'use-tool:read' }] }],
  });
  assert.equal(status, 200, 'ai-sdk stream accepted');

  const start = frames.find((f) => f.type === 'tool-input-start');
  assert.ok(start, 'ai-sdk emitted tool-input-start');
  const callId = start.toolCallId;
  const deltas = frames.filter((f) => f.type === 'tool-input-delta' && f.toolCallId === callId);
  assert.ok(deltas.length >= 1, 'ai-sdk emitted live tool-input-delta');
  const joined = deltas.map((f) => f.inputTextDelta).join('');
  assert.equal(joined, EXPECTED_ARGS, 'ai-sdk delta suffixes concatenate to the tool input');

  const available = frames.find((f) => f.type === 'tool-input-available' && f.toolCallId === callId);
  assert.ok(available, 'ai-sdk closed with tool-input-available');
  assert.equal(frames.filter((f) => f.type === 'tool-input-available' && f.toolCallId === callId).length, 1);
  assert.ok(frames.indexOf(start) < frames.indexOf(deltas[0]), 'ai-sdk starts before live input');
  assert.ok(frames.indexOf(deltas.at(-1)) < frames.indexOf(available), 'ai-sdk closes after live input');
  assert.deepEqual(available.input, { pattern: '*.md' }, 'ai-sdk tool-input-available carries the parsed input');
  pass(`ai-sdk streams tool-call args (${deltas.length} live tool-input-delta frames)`);
  return deltas.length;
}

// Causes: G1 provider fragments may coalesce before the AG-UI encoder; G2 the
// tool call closes. Effects: E1 at least one live TOOL_CALL_ARGS precedes
// TOOL_CALL_END, E2 its bytes concatenate to the exact input. Rules G1=>E1+E2
// regardless of wire frame count, G2=>one terminal call event.
async function checkAgUi(base) {
  const { status, frames } = await postSse(`${base}/v1/ag-ui`, {
    threadId: 'agui-stream',
    runId: 'run-1',
    messages: [{ id: 'u1', role: 'user', content: 'use-tool:read' }],
  });
  assert.equal(status, 200, 'ag-ui stream accepted');

  const start = frames.find((f) => f.type === 'TOOL_CALL_START');
  assert.ok(start, 'ag-ui emitted TOOL_CALL_START');
  const callId = start.toolCallId;
  const args = frames.filter((f) => f.type === 'TOOL_CALL_ARGS' && f.toolCallId === callId);
  assert.ok(args.length >= 1, 'ag-ui emitted live TOOL_CALL_ARGS');
  const joined = args.map((f) => f.delta).join('');
  assert.equal(joined, EXPECTED_ARGS, 'ag-ui arg deltas concatenate to the tool input');
  const end = frames.find((f) => f.type === 'TOOL_CALL_END' && f.toolCallId === callId);
  assert.ok(end, 'ag-ui closed the streamed tool call with TOOL_CALL_END');
  assert.equal(frames.filter((f) => f.type === 'TOOL_CALL_END' && f.toolCallId === callId).length, 1);
  assert.ok(frames.indexOf(start) < frames.indexOf(args[0]), 'ag-ui starts before live args');
  assert.ok(frames.indexOf(args.at(-1)) < frames.indexOf(end), 'ag-ui closes after live args');
  pass(`ag-ui streams tool-call args (${args.length} live TOOL_CALL_ARGS frames)`);
  return args.length;
}

// Verify A2A is request/response: `message/send` returns a single JSON Task, not
// an SSE stream of argument deltas. This is the by-design A2A behavior (the
// binding omits streaming), so "streaming tool calls" is N/A for this protocol.
async function checkA2a(base) {
  const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
  const res = await client.sendMessage({
    message: {
      messageId: 'm1',
      contextId: 'a2a-stream',
      role: 'user',
      kind: 'message',
      parts: [{ kind: 'text', text: 'use-tool:read' }],
    },
  });
  // A unary JSON-RPC result — a single Task object, never a frame stream.
  assert.ok(res.result, 'a2a message:send returned a single JSON-RPC result');
  assert.ok(res.result.kind === 'task' || res.result.status, 'a2a result is a Task (request/response)');
  pass('a2a is request/response: message:send returns one Task, no incremental tool-arg stream (streaming omitted by design)');
}

async function main() {
  try {
    await withRealServer('default', PORT, async (base) => {
      const sdk = await checkAiSdk(base);
      const agui = await checkAgUi(base);
      await checkA2a(base);

      console.log('\nStreaming tool-call support matrix:');
      console.log(`  ai-sdk : YES — ${sdk} live tool-input-delta frames`);
      console.log(`  ag-ui  : YES — ${agui} live TOOL_CALL_ARGS frames`);
      console.log('  a2a    : N/A — request/response Task (streaming omitted from the A2A binding)');
    });
    console.log('\nE2E PASS: streaming tool calls verified across ai-sdk, ag-ui, and a2a.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
