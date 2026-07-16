// A session inherits its published agent's model from the config plane (Managed
// Agents compatibility, Part A), driven through the official Anthropic TS SDK.
//
// The config plane owns an agent's model/system/tools. When a session references a
// published agent WITHOUT overriding the model, it must run that agent's
// authoritative model — the same truth `/v1/agents` projects — not the host default.
// The session path reads it through the existing `AgentConfigSource` port (the one
// `/v1/agents` already uses), not a second source. An `agent_with_overrides.model`
// still wins over the inherited model.
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
        headers: { 'content-type': 'application/json' },
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

    // ── override still wins over the inherited model ─────────────────────────────
    const overridden = await client.beta.sessions.create({
      agent: { id: AGENT, type: 'agent_with_overrides', model: 'override-model' },
      environment_id: 'env_local',
      betas: BETAS,
    });
    assert.equal(overridden.agent.model.id, 'override-model', 'agent_with_overrides.model beats inheritance');
    pass('agent_with_overrides.model overrides the inherited config-plane model');
  });

  console.log('E2E PASS: session inherits published agent model (config plane) + override wins.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
