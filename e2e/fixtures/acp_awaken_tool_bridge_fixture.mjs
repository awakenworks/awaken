#!/usr/bin/env node

import readline from 'node:readline';

const TOOL_CALL_ID = 'awaken-write-call';
const WRITE_ARGUMENTS = { path: '/mnt/session/outputs/acp-hitl.txt', content: 'AWAKEN-ACP-HITL-OK' };
let sessionRequest;

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

async function mcpRequest(url, method, params, state) {
  const headers = {
    accept: 'application/json, text/event-stream',
    'content-type': 'application/json',
    ...(state.sessionId ? { 'mcp-session-id': state.sessionId } : {}),
  };
  const response = await fetch(url, {
    method: 'POST',
    headers,
    body: JSON.stringify({ jsonrpc: '2.0', id: state.nextId++, method, params }),
  });
  if (!response.ok) throw new Error(`MCP ${method} returned ${response.status}: ${await response.text()}`);
  state.sessionId ??= response.headers.get('mcp-session-id');
  const body = await response.text();
  const payload = body.split('\n')
    .map((line) => line.trim())
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trim())
    .find((line) => line && line !== '[DONE]') ?? body;
  const decoded = JSON.parse(payload);
  if (decoded.error) throw new Error(`MCP ${method} failed: ${JSON.stringify(decoded.error)}`);
  return decoded.result;
}

async function mcpNotify(url, method, state) {
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      accept: 'application/json, text/event-stream',
      'content-type': 'application/json',
      ...(state.sessionId ? { 'mcp-session-id': state.sessionId } : {}),
    },
    body: JSON.stringify({ jsonrpc: '2.0', method }),
  });
  if (!response.ok) throw new Error(`MCP ${method} returned ${response.status}: ${await response.text()}`);
}

async function callAwakenTools() {
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
  await mcpRequest(url, 'tools/call', { name: 'write', arguments: WRITE_ARGUMENTS }, state);
  const read = await mcpRequest(url, 'tools/call', {
    name: 'read',
    arguments: { path: WRITE_ARGUMENTS.path },
  }, state);
  const rendered = JSON.stringify(read);
  if (!rendered.includes(WRITE_ARGUMENTS.content)) {
    throw new Error(`read-back did not contain marker: ${rendered}`);
  }
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
    sessionRequest = request;
    send({ jsonrpc: '2.0', id: request.id, result: { sessionId: 'awaken-tool-bridge-session' } });
    return;
  }
  if (request.method === 'session/prompt') {
    emitToolCall();
    return;
  }
  if (request.id !== 42) return;
  const outcome = request.result?.outcome;
  if (outcome?.outcome === 'selected' && outcome.optionId === 'ok') {
    try {
      await callAwakenTools();
      send({ jsonrpc: '2.0', method: 'session/update', params: {
        sessionId: 'awaken-tool-bridge-session',
        update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'ACP-BRIDGE-ALLOWED-READBACK-OK' } },
      } });
      send({ jsonrpc: '2.0', id: 3, result: { stopReason: 'end_turn' } });
    } catch (error) {
      process.stderr.write(`${error.stack ?? error}\n`);
      process.exitCode = 1;
    }
    return;
  }
  if (outcome?.outcome === 'selected' && outcome.optionId === 'no') {
    send({ jsonrpc: '2.0', method: 'session/update', params: {
      sessionId: 'awaken-tool-bridge-session',
      update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'ACP-BRIDGE-DENIED-NO-EFFECT' } },
    } });
    send({ jsonrpc: '2.0', id: 3, result: { stopReason: 'end_turn' } });
    return;
  }
  process.exit(0);
});
