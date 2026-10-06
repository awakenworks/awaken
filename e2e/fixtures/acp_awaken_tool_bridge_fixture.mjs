#!/usr/bin/env node

import readline from 'node:readline';
import assert from 'node:assert/strict';
import { mcpRequest, mcpNotify } from './mcp_http_fixture_client.mjs';

const TOOL_CALL_ID = 'awaken-write-call';
const WRITE_ARGUMENTS = { path: '/mnt/session/outputs/acp-hitl.txt', content: 'AWAKEN-ACP-HITL-OK' };
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
    // validates delivery; claude_sdk_alias_e2e separately runs the real SDK lookup.
    assert.deepEqual(request.params._meta?.claudeCode?.options, {
      toolAliases: { Write: 'mcp__awaken_session__write' }, tools: [], disallowedTools: ['Write'],
    });
    sessionRequest = request;
    send({ jsonrpc: '2.0', id: request.id, result: { sessionId: 'awaken-tool-bridge-session' } });
    return;
  }
  if (request.method === 'session/prompt') {
    const prompt = request.params?.prompt?.map((block) => block.text ?? '').join('\n') ?? '';
    if (prompt.includes('Authoritative Awaken tool continuation results')) {
      // The RuntimeToolBatch already settled the reserved Awaken write. The
      // cancelled ACP callback is not a second execution authority.
      assert.ok(prompt.includes(`"call_id":"${TOOL_CALL_ID}"`));
      const allowed = prompt.includes('"is_error":false');
      const denied = prompt.includes('"is_error":true');
      assert.notEqual(allowed, denied, 'the committed owned-tool receipt has one outcome');
      if (allowed) {
        try {
          await readAwakenOutput();
          finishPrompt(request.id, 'ACP-BRIDGE-ALLOWED-READBACK-OK');
        } catch (error) {
          process.stderr.write(`${error.stack ?? error}\n`);
          process.exitCode = 1;
        }
      } else {
        finishPrompt(request.id, 'ACP-BRIDGE-DENIED-NO-EFFECT');
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
