// Cross-protocol verification: does each wire protocol stream a tool call's
// arguments incrementally, or deliver them whole?
//
// Drives the REAL provider path (GenaiExecutor streaming) against the fake
// Anthropic upstream, which chunks `input_json_delta` exactly as Anthropic does.
// The `use-tool:read` prompt makes the model emit a `read` tool call with input
// `{"pattern":"*.md"}`; each adapter is then checked over its own wire:
//
//   - ai-sdk : tool-input-start + N×tool-input-delta + tool-input-available
//   - ag-ui  : TOOL_CALL_START + N×TOOL_CALL_ARGS + TOOL_CALL_END
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

// Verify the ai-sdk UI Message Stream carried the tool input as incremental
// deltas, closed by the authoritative parsed `tool-input-available`.
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
  assert.ok(deltas.length >= 2, `ai-sdk streamed the args incrementally (got ${deltas.length} deltas)`);
  const joined = deltas.map((f) => f.inputTextDelta).join('');
  assert.equal(joined, EXPECTED_ARGS, 'ai-sdk delta suffixes concatenate to the tool input');

  const available = frames.find((f) => f.type === 'tool-input-available' && f.toolCallId === callId);
  assert.ok(available, 'ai-sdk closed with tool-input-available');
  assert.deepEqual(available.input, { pattern: '*.md' }, 'ai-sdk tool-input-available carries the parsed input');
  pass(`ai-sdk streams tool-call args incrementally (${deltas.length} tool-input-delta frames)`);
  return deltas.length;
}

// Verify the AG-UI event stream carried the tool args as incremental
// TOOL_CALL_ARGS deltas, closed by TOOL_CALL_END.
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
  assert.ok(args.length >= 2, `ag-ui streamed the args incrementally (got ${args.length} deltas)`);
  const joined = args.map((f) => f.delta).join('');
  assert.equal(joined, EXPECTED_ARGS, 'ag-ui arg deltas concatenate to the tool input');
  assert.ok(
    frames.some((f) => f.type === 'TOOL_CALL_END' && f.toolCallId === callId),
    'ag-ui closed the streamed tool call with TOOL_CALL_END',
  );
  pass(`ag-ui streams tool-call args incrementally (${args.length} TOOL_CALL_ARGS frames)`);
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
      console.log(`  ai-sdk : YES — ${sdk} incremental tool-input-delta frames`);
      console.log(`  ag-ui  : YES — ${agui} incremental TOOL_CALL_ARGS frames`);
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
