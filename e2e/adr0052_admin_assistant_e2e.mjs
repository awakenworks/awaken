// ADR-0052 end-to-end: the management ("admin") assistant, over the REAL config
// server (`config` scenario: in-memory SQLite config store + the scope-keyed config
// plane, with the assistant seeded into the reserved scope and a model resolver).
//
// It drives the HTTP surface the ADR changed:
//   1. The assistant is an ordinary published agent, projected on `/v1/agents` (D1),
//      carrying the four admin tools and an auto-resolved model (D3/D5).
//   2. Scope-keyed tool visibility (D3): a config naming an `admin_*` tool validates
//      only in the reserved scope (via `/v1/workspaces/__admin/...`), and is rejected
//      (UnknownTool) in the tenant/default scope — the tool's existence is not even
//      disclosed there.
//   3. Model selection wire (D5): `{"mode":"auto"}` publishes (resolved to a concrete
//      binding), and the historic flat triple still publishes as a pin (back-compat).
//
// The tools' *execution* by the assistant (drafting/validating) is covered by the
// Rust unit tests — the config scenario's echo model does not emit tool calls.
//
// Run: (from e2e/)  node adr0052_admin_assistant_e2e.mjs

import assert from 'node:assert/strict';
import { withScenarioServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38290);
const RESERVED = '__admin';
const ASSISTANT = '__admin_assistant';
const ADMIN_TOOL = 'admin_get_platform_capabilities';

async function main() {
  await withScenarioServer('config', 'instruction', PORT, async (baseUrl) => {
    const json = async (method, path, body) => {
      const res = await fetch(`${baseUrl}${path}`, {
        method,
        headers: { 'content-type': 'application/json' },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      return { status: res.status, body: await res.json().catch(() => ({})) };
    };

    // 1. The seeded assistant is projected on /v1/agents as the config truth (D1/D2).
    const projected = await json('GET', `/v1/agents/${ASSISTANT}`);
    assert.equal(projected.status, 200, 'the management assistant is a published, projectable agent');
    assert.ok(projected.body.model && projected.body.model.id, 'its model auto-resolved (D5)');
    const toolIds = (projected.body.tools ?? []).map((t) => t.name ?? t.id ?? t);
    for (const id of [
      'admin_get_platform_capabilities',
      'admin_create_agent_draft',
      'admin_set_plugin_config',
      'admin_validate_agent',
    ]) {
      assert.ok(toolIds.includes(id), `assistant carries ${id}`);
    }
    pass(`assistant projected with 4 admin tools + auto-bound model ${projected.body.model.id}`);

    // 2a. A tenant/default-scope config naming an admin tool is rejected (D3): the
    // admin tool is not in the default scope's catalog, so compile fails closed.
    const namingAdmin = {
      id: 'sneaky',
      instructions: 'try to use an admin tool',
      max_steps: 4,
      model_binding: { mode: 'auto' },
      tool_ids: [ADMIN_TOOL],
      plugin_ids: [],
      plugin_config: {},
    };
    const tenantValidate = await json('POST', `/v1/config/agents/sneaky/validate`, namingAdmin);
    assert.equal(tenantValidate.status, 400, 'default scope rejects a config naming an admin tool');
    assert.match(
      JSON.stringify(tenantValidate.body),
      /unknown tool/i,
      'rejection is UnknownTool (fail-closed, existence not disclosed)',
    );
    pass('tenant scope: admin tool is unknown → config rejected');

    // 2b. The SAME config validates in the reserved scope, reached via the
    // workspace-path rewrite that stamps the reserved scope (D2/D3).
    const reservedValidate = await json(
      'POST',
      `/v1/workspaces/${RESERVED}/config/agents/sneaky/validate`,
      namingAdmin,
    );
    assert.equal(reservedValidate.status, 200, 'reserved scope resolves the admin tool');
    assert.equal(reservedValidate.body.valid, true, 'config compiles in the reserved scope');
    pass('reserved scope: admin tool resolves → config validates');

    // 2c. And it PUBLISHES in the reserved scope (the full author→publish path), then
    // projects with the admin tool — proving a reserved-scope agent carrying a
    // management tool is a first-class published agent (D1/D3).
    assert.equal(
      (await json('PUT', `/v1/workspaces/${RESERVED}/config/agents/sneaky`, namingAdmin)).status,
      200,
      'stored in the reserved scope',
    );
    const reservedPub = await json('POST', `/v1/workspaces/${RESERVED}/config/agents/sneaky/publish`, undefined);
    assert.equal(reservedPub.status, 200, 'admin-tool config publishes in the reserved scope');
    const sneakyProjected = await json('GET', `/v1/agents/sneaky`);
    const sneakyTools = (sneakyProjected.body.tools ?? []).map((t) => t.name ?? t.id ?? t);
    assert.ok(sneakyTools.includes(ADMIN_TOOL), 'the published reserved-scope agent carries the admin tool');
    pass('reserved scope: admin-tool config publishes + projects');

    // 3a. Model selection wire (D5): an {"mode":"auto"} config publishes — the
    // resolver binds a concrete first-offering at publish.
    const autoAgent = {
      id: 'auto-agent',
      instructions: 'auto model',
      max_steps: 4,
      model_binding: { mode: 'auto' },
      tool_ids: [],
      plugin_ids: [],
      plugin_config: {},
    };
    assert.equal((await json('PUT', `/v1/config/agents/auto-agent`, autoAgent)).status, 200, 'stored');
    const autoPub = await json('POST', `/v1/config/agents/auto-agent/publish`, undefined);
    assert.equal(autoPub.status, 200, 'auto-bound config publishes (resolver bound a model)');
    assert.ok(autoPub.body.fingerprint, 'publication is content-addressed');
    const autoProjected = await json('GET', `/v1/agents/auto-agent`);
    assert.ok(autoProjected.body.model.id, 'auto model resolved to a concrete id');
    pass(`auto binding publishes + resolves to model ${autoProjected.body.model.id}`);

    // 3b. Back-compat: the historic flat triple still publishes (as a pin).
    const pinnedAgent = {
      id: 'pinned-agent',
      instructions: 'pinned model',
      max_steps: 4,
      model_binding: { provider_identity_ref: 'default', model_ref: 'pinned-model', backend_ref: 'default' },
      tool_ids: [],
      plugin_ids: [],
      plugin_config: {},
    };
    assert.equal((await json('PUT', `/v1/config/agents/pinned-agent`, pinnedAgent)).status, 200, 'stored');
    assert.equal(
      (await json('POST', `/v1/config/agents/pinned-agent/publish`, undefined)).status,
      200,
      'flat-triple (pinned) config still publishes (back-compat)',
    );
    const pinnedProjected = await json('GET', `/v1/agents/pinned-agent`);
    assert.equal(pinnedProjected.body.model.id, 'pinned-model', 'pinned model is used verbatim');
    pass('back-compat: historic flat model_binding publishes as a pin');
  });

  console.log('\nE2E PASS: ADR-0052 management assistant verified (projection, scope fence, model selection).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
