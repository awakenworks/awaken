// Session model axis (Managed Agents compatibility), driven through the official
// Anthropic TS SDK. The model lives on the official `agent` object — a session
// inherits it, and the SDK's `agent_with_overrides.model` replaces it for one
// session — rather than on a private metadata key. Credential stays host-side, so
// the wire carries only a model id (no `credential_source_id`).
//
// Covers: plain reference echoes the host model at version 1; a pinned `version` is
// echoed; `agent_with_overrides.model` (string and `{id, speed}` object) replaces
// the session model and echoes the pinned version; `model: null` is rejected 400
// (a session always needs a model — the API's `agent_model_required`).
//
// Deterministic (management mode, no key).

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT) || 38166;

async function main() {
  await withScenarioServer('management', 'mcp', PORT, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // ── plain reference: the session echoes the host default model, version 1 ────
    const bare = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
    const hostModel = bare.agent.model.id;
    assert.ok(hostModel && hostModel.length > 0, `plain ref echoes a model id: ${hostModel}`);
    assert.equal(bare.agent.version, 1, 'plain string ref defaults to version 1');
    pass(`plain reference echoes the host model (${hostModel}) at version 1`);

    // ── pinned version (no override): the version is echoed, model unchanged ──────
    const pinned = await client.beta.sessions.create({
      agent: { id: 'assistant', type: 'agent', version: 7 },
      betas: BETAS,
    });
    assert.equal(pinned.agent.version, 7, 'pinned version is echoed');
    assert.equal(pinned.agent.model.id, hostModel, 'no override keeps the host model');
    pass('pinned agent version is echoed, model inherited');

    // ── agent_with_overrides.model (string): replaces the session model ──────────
    const overStr = await client.beta.sessions.create({
      agent: { id: 'assistant', type: 'agent_with_overrides', model: 'claude-sonnet-5' },
      betas: BETAS,
    });
    assert.equal(overStr.agent.model.id, 'claude-sonnet-5', 'string override replaces the model');
    assert.notEqual(overStr.agent.model.id, hostModel, 'override differs from the host model');
    pass('agent_with_overrides.model (string) replaces the session model');

    // ── agent_with_overrides.model ({id, speed}) with a pinned version ───────────
    const overObj = await client.beta.sessions.create({
      agent: {
        id: 'assistant',
        type: 'agent_with_overrides',
        version: 4,
        model: { id: 'claude-opus-4-8', speed: 'fast' },
      },
      betas: BETAS,
    });
    assert.equal(overObj.agent.model.id, 'claude-opus-4-8', 'object override id replaces the model');
    assert.equal(overObj.agent.model.speed, 'fast', 'object override carries speed through');
    assert.equal(overObj.agent.version, 4, 'override echoes its pinned base version');
    pass('agent_with_overrides.model ({id, speed}) replaces the model and echoes version');

    // ── model: null is a clear — rejected, a session always needs a model ────────
    // Raw POST: the not-clearable rule is a wire-level constraint, so drive it past
    // the SDK's typed params directly.
    const res = await fetch(`${base}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
      body: JSON.stringify({
        agent: { id: 'assistant', type: 'agent_with_overrides', model: null },
      }),
    });
    assert.equal(res.status, 400, `clearing the model is a 400 (got ${res.status})`);
    const body = await res.json();
    assert.equal(body.type, 'error', 'error envelope shape');
    assert.equal(body.error.type, 'invalid_request_error', 'invalid_request_error type');
    pass('model: null (clear) is rejected 400 — a session always needs a model');
  });

  console.log('E2E PASS: session model axis (official agent.model + agent_with_overrides).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
