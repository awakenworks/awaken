// Session model axis (Managed Agents compatibility), driven through the official
// Anthropic TS SDK. The model lives on the official `agent` object — a session
// inherits it, and the SDK's `agent_with_overrides.model` replaces it for one
// session — rather than on a private metadata key. Credential stays host-side, so
// the wire carries only a model id (no `credential_source_id`).
//
// Covers: plain reference echoes the host model at version 1; an unavailable
// pinned `version` fails closed; `agent_with_overrides.model` (string and
// `{id, speed}` object) replaces the session model; `model: null` is rejected 400.
//
// Deterministic (management mode, no key).

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT) || 38166;
const ENVIRONMENT_ID = 'env_local';

async function main() {
  const fixture = await startCalcFixture('unused-anonymous-token', { allowAnonymous: true }); // awaken-allow: secret
  try {
    await withScenarioServer('management', 'mcp', PORT, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // ── plain reference: the session echoes the host default model, version 1 ────
    const bare = await client.beta.sessions.create({
      agent: 'assistant', environment_id: ENVIRONMENT_ID, betas: BETAS,
    });
    const hostModel = bare.agent.model.id;
    assert.ok(hostModel && hostModel.length > 0, `plain ref echoes a model id: ${hostModel}`);
    assert.equal(bare.agent.version, 1, 'plain string ref defaults to version 1');
    pass(`plain reference echoes the host model (${hostModel}) at version 1`);

    // ── unavailable pinned version fails before Session creation ────────────────
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: { id: 'assistant', type: 'agent', version: 7 },
        environment_id: ENVIRONMENT_ID,
        betas: BETAS,
      }),
      (error) => error.status === 400 && error.message.includes('agent_version_unavailable'),
    );
    pass('unavailable pinned agent version fails closed');

    // ── agent_with_overrides.model (string): replaces the session model ──────────
    const overStr = await client.beta.sessions.create({
      agent: { id: 'assistant', type: 'agent_with_overrides', model: 'claude-sonnet-5' },
      environment_id: ENVIRONMENT_ID,
      betas: BETAS,
    });
    assert.equal(overStr.agent.model.id, 'claude-sonnet-5', 'string override replaces the model');
    assert.notEqual(overStr.agent.model.id, hostModel, 'override differs from the host model');
    pass('agent_with_overrides.model (string) replaces the session model');

    // ── agent_with_overrides.model ({id, speed}) ────────────────────────────────
    const overObj = await client.beta.sessions.create({
      agent: {
        id: 'assistant',
        type: 'agent_with_overrides',
        model: { id: 'claude-opus-4-8', speed: 'fast' },
      },
      environment_id: ENVIRONMENT_ID,
      betas: BETAS,
    });
    assert.equal(overObj.agent.model.id, 'claude-opus-4-8', 'object override id replaces the model');
    assert.equal(overObj.agent.model.speed, 'fast', 'object override carries speed through');
    pass('agent_with_overrides.model ({id, speed}) replaces the model');

    // Official create-time override semantics: null/empty arrays clear the
    // session-local field. This must not mutate the underlying Agent version.
    const cleared = await client.beta.sessions.create({
      agent: {
        id: 'assistant',
        type: 'agent_with_overrides',
        system: null,
        tools: [],
        mcp_servers: [],
        skills: [],
      },
      environment_id: ENVIRONMENT_ID,
      betas: BETAS,
    });
    assert.equal(cleared.agent.system, null);
    assert.deepEqual(cleared.agent.tools, []);
    assert.deepEqual(cleared.agent.mcp_servers, []);
    assert.deepEqual(cleared.agent.skills, []);
    pass('create-time null/empty override fields clear only the session');

    // Cause-effect graph for create-time MCP replacement:
    // C1 override omitted -> E1 inherit Agent declarations
    // C1 present + C2 empty -> E2 clear Agent declarations
    // C1 present + !C2 + C3 referenced by toolset -> E3 replace and realize
    // C1 present + !C2 + !C3 -> E4 reject the dangling declaration
    //
    // | Rule | Override | Empty | Referenced | Result  |
    // | M1   | omitted  | -     | -          | inherit |
    // | M2   | present  | yes   | -          | clear   |
    // | M3   | present  | no    | yes        | replace |
    // | M4   | present  | no    | no         | reject  |
    const mcpFiltered = await client.beta.sessions.create({
      agent: {
        id: 'assistant',
        type: 'agent_with_overrides',
        tools: [{
          type: 'mcp_toolset',
          mcp_server_name: 'calc',
          default_config: { enabled: false },
          configs: [{ name: 'add', enabled: true }],
        }],
        mcp_servers: [{ type: 'url', name: 'calc', url: fixture.url }],
      },
      environment_id: ENVIRONMENT_ID,
      betas: BETAS,
    });
    assert.equal(mcpFiltered.agent.tools[0].type, 'mcp_toolset');
    assert.equal(mcpFiltered.agent.tools[0].mcp_server_name, 'calc');
    assert.equal(mcpFiltered.agent.tools[0].default_config.enabled, false);
    assert.deepEqual(mcpFiltered.agent.mcp_servers, [{
      type: 'url', name: 'calc', url: fixture.url,
    }]);
    pass('M3: mcp_toolset allowlist replaces and realizes its declared MCP server');

    await assert.rejects(
      () => client.beta.sessions.create({
        agent: {
          id: 'assistant',
          type: 'agent_with_overrides',
          mcp_servers: [{ type: 'url', name: 'dangling', url: 'http://127.0.0.1:1/mcp' }],
          tools: [],
        },
        environment_id: ENVIRONMENT_ID,
        betas: BETAS,
      }),
      (err) => err.status === 400,
    );
    pass('M4: unreferenced MCP server is rejected 400');

    // ── model: null is a clear — rejected, a session always needs a model ────────
    // Raw POST: the not-clearable rule is a wire-level constraint, so drive it past
    // the SDK's typed params directly.
    const res = await fetch(`${base}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: JSON.stringify({
        agent: { id: 'assistant', type: 'agent_with_overrides', model: null },
        environment_id: ENVIRONMENT_ID,
      }),
    });
    assert.equal(res.status, 400, `clearing the model is a 400 (got ${res.status})`);
    const body = await res.json();
    assert.equal(body.type, 'error', 'error envelope shape');
    assert.equal(body.error.type, 'invalid_request_error', 'invalid_request_error type');
    pass('model: null (clear) is rejected 400 — a session always needs a model');
    });
  } finally {
    await fixture.close();
  }

  console.log('E2E PASS: session model axis (official agent.model + agent_with_overrides).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
