// Official-wire ACP e2e: a `runtime:"acp:*"` session drives the fake agent over
// the REAL agent-client-protocol JSON-RPC 2.0 codec (not the newline stand-in) —
// the `Codec::Acp` driver runs the full handshake (`initialize` → `session/new`
// → `session/prompt`), projects a `session/update` agent-message chunk into the
// transcript, and ends on the prompt's `stopReason`. Proves the production codec
// end-to-end through the Managed API. `AWAKEN_MODEL_MODE=acp-jsonrpc`.
//
// Run: (from e2e/)  node acp_jsonrpc_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events;
}

async function agentTexts(client, sessionId) {
  return (await listEvents(client, sessionId))
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function send(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function main() {
  try {
    await withServer('acp-jsonrpc', 38172, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // A session selecting an ACP runtime drives the fake agent over official
      // JSON-RPC — the `session/update` chunk lands as the agent's message.
      const acp = await client.beta.sessions.create({
        agent: 'assistant', metadata: { 'awaken.runtime': 'acp:claude' },
        environment_id: 'env_local',
        betas: BETAS,
      });
      await send(client, acp.id, 'hello');
      let texts = await agentTexts(client, acp.id);
      assert.ok(
        texts.some((t) => t.includes('acp-jsonrpc reply')),
        `official ACP JSON-RPC turn projected the agent message, got ${JSON.stringify(texts)}`,
      );
      pass('runtime:"acp:claude" runs over the official ACP JSON-RPC codec (handshake + prompt + update projection)');

      // The agent's tool_call + its terminal tool_call_update project into the
      // transcript: the external agent's tool result now surfaces (previously
      // dropped by the ACL). Managed sees an agent.tool_use and an agent.tool_result.
      const events = await listEvents(client, acp.id);
      const toolUse = events.find((e) => e.type === 'agent.tool_use' && e.name === 'read');
      assert.ok(toolUse, `the ACP tool call surfaced as agent.tool_use, got ${events.map((e) => e.type)}`);
      const toolResult = events.find((e) => e.type === 'agent.tool_result');
      assert.ok(toolResult, `the ACP tool result surfaced as agent.tool_result, got ${events.map((e) => e.type)}`);
      const resultText = (toolResult.content ?? []).map((c) => c.text ?? '').join('');
      assert.ok(
        resultText.includes('file body'),
        `the external agent's tool output reached the transcript, got ${JSON.stringify(toolResult.content)}`,
      );
      pass('an ACP tool call and its result project into the transcript (agent.tool_use + agent.tool_result)');

      // A second turn relaunches the CLI and completes another JSON-RPC handshake.
      await send(client, acp.id, 'again');
      texts = await agentTexts(client, acp.id);
      assert.ok(
        texts.filter((t) => t.includes('acp-jsonrpc reply')).length >= 2,
        `a second JSON-RPC turn completed, got ${JSON.stringify(texts)}`,
      );
      pass('a second turn relaunches the agent and re-runs the JSON-RPC handshake (R7)');

      // Selection still holds: a native session on the same server runs the model.
      const native = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await send(client, native.id, 'hello');
      texts = await agentTexts(client, native.id);
      assert.ok(
        texts.some((t) => t.startsWith('Echo:')),
        `native session ran the built-in model, got ${JSON.stringify(texts)}`,
      );
      pass('native sessions on the same server still run the built-in model');
    });

    console.log('E2E PASS: official ACP JSON-RPC codec end-to-end via the managed API.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
