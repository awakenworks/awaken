// Project consumption-side ingress e2e (ADR-0042 amendment): each project has
// a unique access path (`/projects/{id}`) the STOCK Anthropic SDK reaches by
// baseURL alone — no wire change. The same agent id gets a different MCP tool
// surface per project (project binding), while the bare path keeps the
// workspace-level behavior. Authority still flows from the API key; the path
// segment is addressing only.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-project-bearer-token'; // awaken-allow: secret

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function sendMessage(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
}

function agentMessages(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((e) => e.content.map((b) => b.text ?? '').join(''));
}

async function main() {
  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withServer('management', 38193, async (baseUrl) => {
      const admin = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // --- supply (workspace-owned): credential + MCP server def ------------
      let res = await fetch(`${baseUrl}/v1/config/credentials`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ workspace_id: 'ws', kind: 'vault', secret: CALC_TOKEN }),
      });
      assert.equal(res.status, 201);
      const sourceId = (await res.json()).id;
      res = await fetch(`${baseUrl}/v1/config/mcp-servers/calc-def`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          id: 'calc-def',
          display_name: 'calc',
          url: fixture.url,
          credential_binding: { type: 'exact', credential_source_id: sourceId },
          version: 1,
        }),
      });
      assert.equal(res.status, 200);
      pass('supply authored: credential + McpServerDef (workspace-owned, no copies)');

      // --- two projects; only `tools` binds calc to the agent ---------------
      for (const id of ['tools', 'bare']) {
        res = await fetch(`${baseUrl}/v1/config/projects/${id}`, {
          method: 'PUT',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ id, workspace_id: 'ws', display_name: id, version: 1 }),
        });
        assert.equal(res.status, 200);
      }
      for (const [id, servers] of [
        ['tools', ['calc-def']],
        ['bare', []],
      ]) {
        res = await fetch(`${baseUrl}/v1/config/projects/${id}/agents/calc-agent/mcp`, {
          method: 'PUT',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            project_id: id,
            agent_id: 'calc-agent',
            mcp_server_ids: servers,
            version: 1,
          }),
        });
        assert.equal(res.status, 200);
      }
      pass('projects authored: `tools` binds calc-def, `bare` binds nothing');

      // --- STOCK SDK, project baseURL: the tool surface follows the project -
      const tools = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `${baseUrl}/projects/tools` });
      const session = await client_session(tools);
      await sendMessage(tools, session, 'add 4 5');
      let events = await listEvents(tools, session);
      assert.ok(
        events.some((e) => e.type === 'agent.tool_use' && e.name === 'mcp__calc__add'),
        `project tools calls mcp__calc__add: ${JSON.stringify(events.map((e) => e.type))}`,
      );
      assert.ok(
        agentMessages(events).some((m) => m.includes('result: 9')),
        'project tools conversation reports result: 9',
      );
      pass('stock SDK @ /projects/tools: same agent id converses via mcp__calc__add');

      // --- same agent id, project `bare`: no MCP tool surface ---------------
      const bare = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `${baseUrl}/projects/bare` });
      const bareSession = await client_session(bare);
      await sendMessage(bare, bareSession, 'add 4 5');
      events = await listEvents(bare, bareSession);
      assert.ok(
        !agentMessages(events).some((m) => m.includes('result: 9')),
        `project bare must not reach the calc server: ${JSON.stringify(agentMessages(events))}`,
      );
      pass('stock SDK @ /projects/bare: same agent id, no MCP tool surface');

      // --- unauthored project: 404 at the ingress ---------------------------
      const ghost = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `${baseUrl}/projects/ghost` });
      await assert.rejects(
        () => client_session(ghost),
        (err) => {
          assert.equal(err.status, 404);
          return true;
        },
      );
      pass('unauthored project path -> 404 before any session exists');

      // --- the bare TOP-LEVEL path is untouched by the project ingress ------
      const workspace = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const wsSession = await client_session(workspace);
      await sendMessage(workspace, wsSession, 'just chatting');
      events = await listEvents(workspace, wsSession);
      assert.ok(agentMessages(events).includes('Echo: just chatting'));
      pass('bare baseURL behavior unchanged (workspace-default surface)');
    });
    // The tools/call bearer came from the vault-backed supply, once per call.
    const toolCalls = fixture.calls.filter((c) => c.method === 'tools/call');
    assert.ok(toolCalls.length >= 1, 'the calc fixture served the tools project');
    assert.ok(
      toolCalls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
      'every tools/call carried the supply credential',
    );
    pass('fixture saw the workspace-owned bearer only from the bound project');
    console.log(
      'E2E PASS: project-scoped MCP consumption via stock-SDK baseURL addressing.',
    );
  } finally {
    fixture.close();
  }
}

async function client_session(client) {
  const session = await client.beta.sessions.create({ agent: 'calc-agent', betas: BETAS });
  return session.id;
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
