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
import {
  managedAgentWithAlwaysAskTools,
  withServer,
  pass,
  waitForSessionEventReceipt,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events;
}

function agentTexts(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function send(client, sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'ACP JSON-RPC Run exact User Event receipt');
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `ACP JSON-RPC Run for ${JSON.stringify(text)} to commit`,
  );
}

function materializedPath(text) {
  const match = text.match(/complete output was written to (.+?)\. Read that file/);
  assert.ok(match, 'tool-result preview must contain a readable spill path: ' + text.slice(-500));
  return match[1];
}

async function main() {
  // Test design (ACP/native arms). Causes: C1=an ACP publication or Native
  // publication receives an accepted User Event; C2=the tool output is inline
  // or oversized; C3=a later turn reuses the Session. Effects: E1=ACP performs
  // initialize/new-or-load/prompt and commits its update; E2=oversized output is
  // preserved behind one readable preview; E3=C3 performs a fresh ACP handshake;
  // E4=Native remains isolated and reaches the same terminal contract.
  // Constraints/invariant: every assertion is scoped after the exact accepted
  // command and one backend may not satisfy another backend's evidence.
  // Decision rules: A1=C1(ACP)+inline=>E1; A2=A1+oversized=>E2;
  // A3=A1+C3=>E3; N1=C1(Native)+oversized=>E2+E4.
  try {
    await withServer('acp-jsonrpc', 38172, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // The published ACP Agent drives the fake agent over official
      // JSON-RPC — the `session/update` chunk lands as the agent's message.
      const acp = await client.beta.sessions.create({
        agent: 'acp-agent',
        environment_id: 'env_local',
        betas: BETAS,
      });
      let observed = await send(client, acp.id, 'hello');
      let texts = agentTexts(observed.events);
      assert.ok(
        texts.some((t) => t.includes('acp-jsonrpc reply')),
        `official ACP JSON-RPC turn projected the agent message, got ${JSON.stringify(texts)}`,
      );
      pass('the published assistant runs over the official ACP JSON-RPC codec (handshake + prompt + update projection)');

      // Cause/effect + decision rules for both execution backends:
      // output <=100k -> inline unchanged; output >100k -> complete sandbox file
      // + bounded preview/path; write failure -> no unmaterialized tool result.
      // A1 exercises ACP projection and N1 below exercises Native execution.
      // Existing Rust tables cover the boundary, Unicode, retry, and failure rows.
      const events = observed.events;
      const toolUse = events.find((e) => e.type === 'agent.tool_use' && e.name === 'read');
      assert.ok(toolUse, `the ACP tool call surfaced as agent.tool_use, got ${events.map((e) => e.type)}`);
      const toolResult = events.find((e) => e.type === 'agent.tool_result');
      assert.ok(toolResult, `the ACP tool result surfaced as agent.tool_result, got ${events.map((e) => e.type)}`);
      const resultText = (toolResult.content ?? []).map((c) => c.text ?? '').join('');
      assert.ok(
        resultText.includes('file body'),
        `the external agent's tool output reached the transcript, got ${JSON.stringify(toolResult.content)}`,
      );
      assert.ok(resultText.length <= 100_000, 'A1 ACP preview is bounded to 100k characters');
      const acpSpillPath = materializedPath(resultText);
      assert.match(acpSpillPath, /^\.awaken\/tool-results\/[0-9a-f]{64}\.txt$/, 'A1 safe relative path');
      assert.ok(
        texts.some((text) => text.includes('acp-spill-readable=100011')),
        `A1 ACP agent must read/count the full spill, got ${JSON.stringify(texts)}`,
      );
      pass('an oversized ACP tool result is stored whole and projected as preview + sandbox path');

      // A second turn relaunches the CLI and completes another JSON-RPC handshake.
      observed = await send(client, acp.id, 'again');
      texts = agentTexts(observed.events);
      assert.ok(
        texts.filter((t) => t.includes('acp-jsonrpc reply')).length >= 2,
        `a second JSON-RPC turn completed, got ${JSON.stringify(texts)}`,
      );
      pass('a second turn relaunches the agent and re-runs the JSON-RPC handshake (R7)');

      // Selection still holds: a native session on the same server runs the model.
      const native = await client.beta.sessions.create({
        agent: managedAgentWithAlwaysAskTools(['bash']),
        environment_id: 'env_local',
        betas: BETAS,
      });
      observed = await send(client, native.id, 'hello');
      texts = agentTexts(observed.events);
      assert.ok(
        texts.some((t) => t.startsWith('Echo:')),
        `native session ran the built-in model, got ${JSON.stringify(texts)}`,
      );
      pass('native sessions on the same server still run the built-in model');

      // N1/H1 cause/effect extension: the canonical scenario now retains the
      // default `ask` gate so its official-wire ACP permission path is testable.
      // Cause=Native oversized Bash call reaches ask; effect=the exact SDK
      // confirmation resumes that same Run and exposes the complete spill.
      // Constraint: no scenario-wide AllowAll shortcut may bypass HITL.
      // Decision rule H1: every ask + allow => successful tool results; no ask =>
      // fail this test instead of silently weakening the permission boundary.
      const nativeReceipt = await client.beta.sessions.events.send(native.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'oversized-tool-output' }] }],
        betas: BETAS,
      });
      let receiptId = nativeReceipt.data[0]?.id;
      assert.equal(typeof receiptId, 'string', 'H1 exact oversized User Event receipt');
      const approvedNativeTools = new Set();
      // Interactive cause/effect rules: H1 receipt -> one ask; H2 exact allow
      // receipt -> next ask; H3 second allow receipt -> end_turn. Effects are
      // read before the next user action; K: predicates never send/drive.
      // Decision H1/H2 missing next effect=>retry; H3 terminal=>complete.
      let nativeObservation;
      for (let expected = 0; expected < 2; expected += 1) {
        nativeObservation = await waitForSessionEventReceipt(
          client,
          native.id,
          receiptId,
          BETAS,
          ({ delta }) => delta.some((event) => event.type === 'agent.tool_use'
            && event.evaluated_permission === 'ask'
            && !approvedNativeTools.has(event.id)),
          `H${expected + 1} Native Bash ask to commit after its exact input`,
        );
        const pendingNativeTool = nativeObservation.delta.find(
          (event) => event.type === 'agent.tool_use'
            && event.evaluated_permission === 'ask'
            && !approvedNativeTools.has(event.id),
        );
        assert.ok(pendingNativeTool, `H${expected + 1} pending Native Bash tool`);
        approvedNativeTools.add(pendingNativeTool.id);
        const confirmation = await client.beta.sessions.events.send(native.id, {
          events: [{
            type: 'user.tool_confirmation',
            tool_use_id: pendingNativeTool.id,
            result: 'allow',
          }],
          betas: BETAS,
        });
        receiptId = confirmation.data[0]?.id;
        assert.equal(typeof receiptId, 'string', `H${expected + 2} exact confirmation receipt`);
      }
      nativeObservation = await waitForSessionEventReceipt(
        client,
        native.id,
        receiptId,
        BETAS,
        ({ delta }) => [...delta].reverse().find(
          (event) => event.type === 'session.status_idle',
        )?.stop_reason?.type === 'end_turn',
        'H3 Native oversized Run to commit end_turn',
      );
      assert.equal(approvedNativeTools.size, 2, 'H1 both Native Bash calls crossed the ask boundary');
      const nativeEvents = nativeObservation.events;
      const nativeToolResult = nativeEvents.find(
        (event) => event.type === 'agent.tool_result'
          && (event.content ?? []).some((content) => (content.text ?? '').includes('Tool output truncated')),
      );
      assert.ok(nativeToolResult, 'N1 Native oversized result surfaced');
      const nativeResultText = (nativeToolResult.content ?? []).map((content) => content.text ?? '').join('');
      assert.ok(nativeResultText.length <= 100_000, 'N1 Native preview is bounded to 100k characters');
      const nativeSpillPath = materializedPath(nativeResultText);
      assert.match(nativeSpillPath, /^\.awaken\/tool-results\/[0-9a-f]{64}\.txt$/, 'N1 safe relative path');
      texts = agentTexts(nativeEvents);
      assert.ok(
        texts.some((text) => text.includes('native oversized tool spill readable bytes=100001')),
        `N1 Native model used a builtin tool to read/count the complete spill, got ${JSON.stringify(texts)}`,
      );
      pass('a Native builtin-tool result is stored whole and consumed through preview + sandbox path');
    });

    console.log('E2E PASS: official ACP JSON-RPC codec end-to-end via the managed API.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
