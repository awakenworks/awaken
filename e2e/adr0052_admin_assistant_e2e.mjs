// ADR-0052 end-to-end: the management ("admin") assistant, over the REAL config
// server (`config` scenario: in-memory SQLite config store + the scope-keyed config
// plane, with the assistant seeded into the reserved scope and a model resolver).
//
// It drives the HTTP surface the ADR changed:
//   1. The assistant is an ordinary published agent, projected on `/v1/agents` (D1),
//      carrying the six admin tools and an auto-resolved model (D3/D5).
//   2. Scope-keyed tool visibility (D3): a config naming an `admin_*` tool validates
//      only in the reserved scope (via `/v1/workspaces/__admin/...`), and is rejected
//      (UnknownTool) in the tenant/default scope — the tool's existence is not even
//      disclosed there.
//   3. Model selection wire (D5): the seeded assistant proves internal auto-resolution;
//      managed model objects and compact model strings publish as explicit pins.
//
// The tools' live execution is covered by adr0052_admin_run_e2e.mjs and the
// production CLI E2E; this scenario focuses on authoring/publication boundaries.
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
      'admin_draft_agent',
      'admin_patch_agent',
      'admin_validate_agent',
      'admin_draft_environment',
      'admin_explain_console',
    ]) {
      assert.ok(toolIds.includes(id), `assistant carries ${id}`);
    }
    pass(`assistant projected with 6 admin tools + auto-bound model ${projected.body.model.id}`);

    // 2a. A tenant/default-scope config naming an admin tool is rejected (D3): the
    // admin tool is not in the default scope's catalog, so compile fails closed.
    const namingAdmin = {
      id: 'sneaky',
      system: 'try to use an admin tool',
      max_steps: 4,
      model: { id: 'fake-haiku' },
      tools: [ADMIN_TOOL],
      plugins: [],
      plugin_config: {},
    };
    const tenantValidate = await json('POST', `/v1/config/agents/sneaky/validate`, namingAdmin);
    assert.equal(tenantValidate.status, 200, 'validation reports semantic failures in its response body');
    assert.equal(tenantValidate.body.valid, false, 'default scope rejects a config naming an admin tool');
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

    // 2c. It PUBLISHES from the reserved authoring namespace into the real platform
    // Workspace supplied by the composition edge. The reserved value is never used
    // as a resource/runtime Workspace (D1/D2/D3).
    assert.equal(
      (await json('PUT', `/v1/workspaces/${RESERVED}/config/agents/sneaky`, namingAdmin)).status,
      200,
      'stored in the reserved scope',
    );
    const reservedPub = await json('POST', `/v1/workspaces/${RESERVED}/config/agents/sneaky/publish`, undefined);
    assert.equal(reservedPub.status, 200, 'admin-tool config publishes from the reserved scope');
    const sneakyProjected = await json('GET', `/v1/agents/sneaky`);
    assert.equal(sneakyProjected.status, 200, 'real platform Workspace owns the installed projection');
    const sneakyTools = (sneakyProjected.body.tools ?? []).map((t) => t.name ?? t.id ?? t);
    assert.ok(sneakyTools.includes(ADMIN_TOOL), 'the published reserved-scope agent carries the admin tool');
    const syntheticWorkspaceProjection = await json('GET', `/v1/workspaces/${RESERVED}/agents/sneaky`);
    assert.equal(
      syntheticWorkspaceProjection.status,
      404,
      'reserved config namespace never becomes a synthetic execution Workspace',
    );
    pass('reserved authoring scope → explicit real execution Workspace; synthetic Workspace stays empty');

    // 3a. Managed model objects become explicit pins. Auto-resolution is exercised
    // by the seeded assistant above, whose internal config uses ModelSelection::Auto.
    const objectModelAgent = {
      id: 'object-model-agent',
      system: 'managed model object',
      max_steps: 4,
      model: { id: 'fake-haiku' },
      tools: [],
      plugins: [],
      plugin_config: {},
    };
    assert.equal(
      (await json('PUT', `/v1/config/agents/object-model-agent`, objectModelAgent)).status,
      200,
      'stored',
    );
    const objectModelPub = await json('POST', `/v1/config/agents/object-model-agent/publish`, undefined);
    assert.equal(objectModelPub.status, 200, 'managed model object publishes');
    assert.ok(objectModelPub.body.fingerprint, 'publication is content-addressed');
    const objectModelProjected = await json('GET', `/v1/agents/object-model-agent`);
    assert.equal(objectModelProjected.body.model.id, 'fake-haiku', 'managed model id is used verbatim');
    pass('managed model object publishes as an explicit pin');

    // 3b. Compact string model input is also accepted as an explicit pin.
    const pinnedAgent = {
      id: 'pinned-agent',
      system: 'pinned model',
      max_steps: 4,
      model: 'pinned-model',
      tools: [],
      plugins: [],
      plugin_config: {},
    };
    assert.equal((await json('PUT', `/v1/config/agents/pinned-agent`, pinnedAgent)).status, 200, 'stored');
    assert.equal(
      (await json('POST', `/v1/config/agents/pinned-agent/publish`, undefined)).status,
      200,
      'compact string model config publishes',
    );
    const pinnedProjected = await json('GET', `/v1/agents/pinned-agent`);
    assert.equal(pinnedProjected.body.model.id, 'pinned-model', 'pinned model is used verbatim');
    pass('compact model string publishes as an explicit pin');
  });

  console.log('\nE2E PASS: ADR-0052 management assistant verified (projection, scope fence, model selection).');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
