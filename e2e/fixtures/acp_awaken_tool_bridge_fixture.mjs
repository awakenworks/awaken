#!/usr/bin/env node

import readline from 'node:readline';
import assert from 'node:assert/strict';
import { mcpRequest, mcpNotify } from './mcp_http_fixture_client.mjs';

const TOOL_CALL_ID = 'awaken-write-call';
const WRITE_ARGUMENTS = { path: '/mnt/session/outputs/acp-hitl.txt', content: 'AWAKEN-ACP-HITL-OK' };
const CONTINUATION_PREFIX = 'Authoritative Awaken tool continuation results';
let sessionRequest;

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

async function readAwakenOutput() {
  const server = sessionRequest?.params?.mcpServers?.find((entry) => entry.name === 'awaken_session');
  const url = server?.url ?? server?.httpUrl;
  if (!url) throw new Error(`Awaken Session MCP route absent: ${JSON.stringify(sessionRequest?.params?.mcpServers)}`);
  const state = { nextId: 1, sessionId: null };
  await mcpRequest(url, 'initialize', {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'awaken-acp-tool-bridge-fixture', version: '1' },
  }, state);
  await mcpNotify(url, 'notifications/initialized', state);
  const read = await mcpRequest(url, 'tools/call', {
    name: 'read',
    arguments: { path: WRITE_ARGUMENTS.path },
  }, state);
  const rendered = JSON.stringify(read);
  if (!rendered.includes(WRITE_ARGUMENTS.content)) {
    throw new Error(`read-back did not contain marker: ${rendered}`);
  }
}

function finishPrompt(id, text) {
  send({ jsonrpc: '2.0', method: 'session/update', params: {
    sessionId: 'awaken-tool-bridge-session',
    update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text } },
  } });
  send({ jsonrpc: '2.0', id, result: { stopReason: 'end_turn' } });
}

function emitToolCall() {
  const rawInput = { server: 'awaken_session', tool: 'write', arguments: WRITE_ARGUMENTS };
  send({
    jsonrpc: '2.0',
    method: 'session/update',
    params: {
      sessionId: 'awaken-tool-bridge-session',
      update: {
        sessionUpdate: 'tool_call',
        toolCallId: TOOL_CALL_ID,
        title: 'mcp.awaken_session.write',
        kind: 'execute',
        status: 'pending',
        rawInput,
      },
    },
  });
  send({
    jsonrpc: '2.0',
    id: 42,
    method: 'session/request_permission',
    params: {
      sessionId: 'awaken-tool-bridge-session',
      toolCall: { toolCallId: TOOL_CALL_ID, title: 'mcp.awaken_session.write', rawInput },
      options: [
        { optionId: 'ok', name: 'Allow', kind: 'allow_once' },
        { optionId: 'no', name: 'Reject', kind: 'reject_once' },
      ],
    },
  });
}

const input = readline.createInterface({ input: process.stdin });
input.on('line', async (line) => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  if (request.method === 'initialize') {
    send({ jsonrpc: '2.0', id: request.id, result: {
      protocolVersion: 1,
      agentCapabilities: { loadSession: true, mcpCapabilities: { http: true } },
    } });
    return;
  }
  if (request.method === 'session/new' || request.method === 'session/load') {
    // R1 new and R2 recovered load retain identical alias policy. This fixture
    // validates delivery of both the declared display spelling and canonical
    // id to the same Session tool. Both SDK lookup names stay disallowed as
    // builtin tools; permissions and execution remain with RuntimeToolBatch.
    // The embedded Node tests reject missing/extra/substituted mappings and
    // builtin/disallowed-list drift before a Session response. The product E2E
    // owns approval/restart/effects; claude_sdk_alias_e2e owns real SDK lookup.
    assert.deepEqual(request.params._meta?.claudeCode?.options, {
      toolAliases: { Write: 'mcp__awaken_session__write', write: 'mcp__awaken_session__write' },
      tools: [], disallowedTools: ['Write', 'write'],
    });
    sessionRequest = request;
    send({ jsonrpc: '2.0', id: request.id, result: { sessionId: 'awaken-tool-bridge-session' } });
    return;
  }
  if (request.method === 'session/prompt') {
    const prompt = request.params?.prompt?.map((block) => block.text ?? '').join('\n') ?? '';
    if (prompt.includes(CONTINUATION_PREFIX)) {
      // Runtime sends its existing committed-order, single-line JSON facts.
      // R3 completed exact write+read -> final without reissuing read; R4 denied
      // write -> denied final without reading. R5 foreign/old/failed/malformed
      // facts cannot prove success. No process-local completion state is used.
      const lines = prompt.split('\n');
      const header = lines.findIndex(line => line.startsWith(CONTINUATION_PREFIX));
      assert.ok(header >= 0, 'the authoritative context has its original header');
      const receipts = JSON.parse(lines[header + 1]);
      assert.ok(Array.isArray(receipts), 'the Runtime context is an array of facts');
      const writeIndex = receipts.findLastIndex(receipt => receipt?.call_id === TOOL_CALL_ID
        && receipt?.tool === 'write');
      assert.ok(writeIndex >= 0, 'the latest owned write occurrence is present');
      const write = receipts[writeIndex];
      assert.deepEqual(write.input, WRITE_ARGUMENTS, 'the owned write retains its canonical input');
      assert.equal(typeof write.is_error, 'boolean', 'the owned outcome is explicit');
      if (write.is_error) {
        finishPrompt(request.id, 'ACP-BRIDGE-DENIED-NO-EFFECT');
        return;
      }
      const read = receipts.slice(writeIndex + 1).findLast(receipt => receipt?.tool === 'read');
      if (read) {
        assert.ok(typeof read.call_id === 'string' && read.call_id.length > 0, 'the read has an occurrence');
        assert.deepEqual(read.input, { path: WRITE_ARGUMENTS.path }, 'the read belongs to the exact output');
        assert.equal(read.is_error, false, 'readback completed successfully');
        assert.ok(Array.isArray(read.content) && read.content.some(block => block?.type === 'text'
          && block.text === WRITE_ARGUMENTS.content), 'the actual read Text contains the exact marker');
        finishPrompt(request.id, 'ACP-BRIDGE-ALLOWED-READBACK-OK');
      } else {
        try {
          // The MCP arrival remains pending while Runtime stops this opaque
          // Step, executes the read and supplies its receipt on the next Step.
          await readAwakenOutput();
        } catch (error) {
          process.stderr.write(`${error.stack ?? error}\n`);
          process.exitCode = 1;
        }
      }
      return;
    }
    emitToolCall();
    return;
  }
  if (request.id !== 42) return;
  const outcome = request.result?.outcome;
  assert.notEqual(outcome?.outcome, 'selected', 'reserved write is executed only by RuntimeToolBatch');
  process.exit(0);
});
