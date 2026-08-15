// A session inherits its published agent's model from the config plane (Managed
// Agents compatibility, Part A), driven through the official Anthropic TS SDK.
//
// The config plane owns an agent's model/system/tools. When a session references a
// published agent WITHOUT overriding the model, it must run that agent's
// authoritative model — the same truth `/v1/agents` projects — not the host default.
// The session path reads it through the existing `AgentConfigSource` port (the one
// `/v1/agents` already uses), not a second source. For a published Agent, an
// `agent_with_overrides.model` may confirm that exact public model id. A host
// without the canonical model resolver fails a different id as unavailable; it
// never splices the id onto the frozen backend/credential route.
//
// Cause/effect graph and decision table:
//   C1 published Agent has model M; C2 Session supplies an override; C3 override=M.
//   C1 -> E1 `/v1/agents` and a plain Session project M from one publication.
//   C1 + C2 + C3 -> E2 accept the same immutable route.
//   C1 + C2 + !C3 + no resolver -> E3 unavailable before Session persistence.
//
//   Rule  C1  C2  C3  Expected
//   M1    Y   N   -   Agent=M, Session=M
//   M2    Y   Y   Y   Agent=M, Session=M
//   M3    Y   Y   N   503 resolver unavailable
//
// Deterministic (config mode). Scoped to the model axis: publishes an agent with a
// known model, then asserts what the session echoes on `session.agent.model`.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT) || 38178;
const AGENT = 'greeter';
const agentConfig = {
  id: AGENT,
  system: 'HELLO',
  max_steps: 4,
  model: { id: 'config-model' },
  tools: [],
  plugins: [],
  plugin_config: {},
};

async function main() {
  await withScenarioServer('config', 'instruction', PORT, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const json = async (method, path, body) => {
      const res = await fetch(`${base}${path}`, {
        method,
        headers: {
          'anthropic-beta': BETAS[0],
          ...(body === undefined ? {} : { 'content-type': 'application/json' }),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      return { status: res.status, body: await res.json().catch(() => ({})) };
    };

    // Author + publish an agent whose config-plane model is `config-model`.
    assert.equal((await json('PUT', `/v1/config/agents/${AGENT}`, agentConfig)).status, 200, 'stored');
    const published = await json('POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(published.status, 200, 'published');
    assert.equal(published.body.installed, true, 'installed into the live catalog');
    // `/v1/agents` projects the config truth (the same port the session reads).
    const projected = await json('GET', `/v1/agents/${AGENT}`, undefined);
    assert.equal(
      projected.status,
      200,
      `published Agent is visible through the beta-gated projection: ${JSON.stringify(projected.body)}`,
    );
    assert.equal(projected.body.model.id, 'config-model', '/v1/agents projects the config model');
    pass('published agent projects model=config-model onto /v1/agents');

    // ── Part A: a plain session reference inherits the published agent's model ────
    const session = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: 'env_local',
      betas: BETAS,
    });
    assert.equal(
      session.agent.model.id,
      'config-model',
      `plain session inherits the published model (got ${session.agent.model.id})`,
    );
    pass('plain session inherits the published agent model from the config plane');

    // M2: an equal public-model override confirms, but cannot change, the frozen
    // execution route.
    const equalOverride = await client.beta.sessions.create({
      agent: { id: AGENT, type: 'agent_with_overrides', model: 'config-model' },
      environment_id: 'env_local',
      betas: BETAS,
    });
    assert.equal(equalOverride.agent.model.id, 'config-model');

    // M3: this config-only scenario intentionally has no model-catalog resolver;
    // a different id must fail unavailable rather than reuse the Agent route.
    await assert.rejects(
      () => client.beta.sessions.create({
        agent: { id: AGENT, type: 'agent_with_overrides', model: 'override-model' },
        environment_id: 'env_local',
        betas: BETAS,
      }),
      (error) => error.status === 503
        && error.message.includes('model override resolution is not configured'),
    );
    pass('config-only host fails alternate model resolution without route splicing');
  });

  console.log('E2E PASS: session inherits the published model; equal override is accepted and missing resolution fails closed.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
