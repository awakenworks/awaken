// An actual Sandbox ACP peer for the Worker-held MCP qualification scenario.
// Protocol requests share the existing ACP fixture's HTTP client.
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import readline from 'node:readline';
import { mcpRequest, mcpNotify } from './mcp_http_fixture_client.mjs';

let launch;
const proofDirectory = process.argv[2] ?? '/mnt/session/outputs';
const send = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
const input = readline.createInterface({ input: process.stdin });
input.on('line', async (line) => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  if (request.method === 'initialize') {
    send({jsonrpc:'2.0', id:request.id, result:{protocolVersion:1,
      agentCapabilities:{loadSession:true, mcpCapabilities:{http:true}}}});
  } else if (['session/new', 'session/load'].includes(request.method)) {
    launch = request.params;
    send({jsonrpc:'2.0', id:request.id, result:{sessionId:'g43-acp'}});
  } else if (request.method === 'session/prompt') {
    try {
      // Causes: C1 exact probe nonce, C2 projected HTTP MCP server without
      // workload credentials, C3 upstream session header preserved. R1 all ->
      // real list/call plus a local proof; R2 missing cause -> ACP error. The
      // proof contains only the workload's own synthetic route and result.
      const text = request.params.prompt.map((part) => part.text ?? '').join('\n');
      const nonce = text.match(/G43_MCP_CALL ([0-9a-f]{32})/)?.[1];
      assert.ok(nonce, 'exact G43 prompt nonce required');
      const server = launch.mcpServers.find((entry) => entry.name === 'g43_mcp');
      const url = server?.url ?? server?.httpUrl;
      assert.ok(url, 'projected G43 MCP route required');
      assert.ok(!server.headers?.some((entry) => /authorization|api.key/i.test(entry.name)),
        'workload receives no upstream credential header');
      const state = {nextId:1, sessionId:null, headers:{'x-awaken-probe-id':nonce}};
      await mcpRequest(url, 'initialize', {protocolVersion:'2025-06-18', capabilities:{},
        clientInfo:{name:'awaken-g43-acp',version:'1'}}, state);
      assert.ok(state.sessionId, 'stateful upstream session id must traverse both hops');
      await mcpNotify(url, 'notifications/initialized', state);
      const tools = await mcpRequest(url, 'tools/list', {}, state);
      assert.ok(tools.tools.some((tool) => tool.name === 'attest'));
      const result = await mcpRequest(url, 'tools/call', {name:'attest', arguments:{nonce}}, state);
      assert.equal(result.isError, false);
      await fs.mkdir(proofDirectory, {recursive:true});
      await fs.writeFile(`${proofDirectory}/g43-${nonce}.json`, JSON.stringify({
        nonce, route:url, upstreamSessionId:state.sessionId, result,
      }), {mode:0o600});
      send({jsonrpc:'2.0', method:'session/update', params:{sessionId:'g43-acp',
        update:{sessionUpdate:'agent_message_chunk', content:{type:'text', text:`G43_MCP_OK ${nonce}`}}}});
      send({jsonrpc:'2.0', id:request.id, result:{stopReason:'end_turn'}});
    } catch {
      send({jsonrpc:'2.0', id:request.id, error:{code:-32000, message:'G43 MCP qualification failed'}});
    }
  }
});
