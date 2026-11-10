#!/usr/bin/env node

import assert from 'node:assert/strict';
import readline from 'node:readline';
import { mcpRequest, mcpNotify } from './mcp_http_fixture_client.mjs';

const sessionId = 'acp-coordination-fixture';
let mcpServers;

function send(message) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
}

function finish(id, text) {
  send({ method: 'session/update', params: { sessionId, update: {
    sessionUpdate: 'agent_message_chunk', content: { type: 'text', text },
  } } });
  send({ id, result: { stopReason: 'end_turn' } });
}

const input = readline.createInterface({ input: process.stdin });
input.on('line', async (line) => {
  const request = JSON.parse(line);
  if (request.method === 'initialize') {
    send({ id: request.id, result: {
      protocolVersion: 1, agentCapabilities: { loadSession: true, mcpCapabilities: { http: true } },
    } });
  } else if (request.method === 'session/new' || request.method === 'session/load') {
    mcpServers = request.params.mcpServers;
    send({ id: request.id, result: { sessionId } });
  } else if (request.method === 'session/prompt') {
    const prompt = request.params.prompt.map((block) => block.text ?? '').join('\n');
    if (prompt.includes('Message from agent researcher')) {
      finish(request.id, 'ACP received the Native child report');
    } else if (prompt.includes('Authoritative Awaken tool continuation results')) {
      assert.match(prompt, /send_message/u);
      assert.match(prompt, /accepted/u);
      finish(request.id, 'ACP accepted the Native child');
    } else if (prompt.includes('coordinate-native-child')) {
      const server = mcpServers?.find((entry) => entry.name === 'awaken_session');
      assert.ok(server?.url ?? server?.httpUrl, 'the sandbox receives the Session MCP route');
      const url = server.url ?? server.httpUrl;
      const state = { nextId: 1, sessionId: null };
      await mcpRequest(url, 'initialize', {
        protocolVersion: '2025-06-18', capabilities: {},
        clientInfo: { name: 'acp-coordination-fixture', version: '1' },
      }, state);
      await mcpNotify(url, 'notifications/initialized', state);
      await mcpRequest(url, 'tools/call', {
        name: 'send_message', arguments: { agent_id: 'researcher', message: 'research task' },
      }, state);
      throw new Error('the Runtime handoff must end this ACP prompt before publishing its receipt');
    } else {
      finish(request.id, 'acp-runtime reply');
    }
  }
});
