// Real DeepSeek + production Awaken + official Managed Agents SDK.
// Only third-party business services are local MCP substitutes. No fake model,
// direct provider tool loop, database assertions, or private execution endpoint.
//
// Cause/effect graph: C1 a live provider publication exists; C2 the Agent names
// real MCP endpoints and matching Vault credentials; C3 Files carry fresh source
// facts; C4 one exact User Event receipt is processed; C5 the model calls tools;
// C6 tool effects and output Files are committed; C7 a fresh SDK client rereads
// the same Session. T1 C1..C7 -> independent business oracle must pass for each
// of the nine templates. T2 missing credentials -> fail, never skip or fake.
// T3 any bad output/tool/provider failure -> preserve evidence, continue other
// cases, and fail the aggregate. T4 scheduled template -> create a real paused
// Deployment, Run once, and inspect its linked Session (clock firing is owned by
// management_deployment_schedule_e2e.mjs). T5 denied MCP write -> no external
// effect, no bypass, exact denial receipt followed by a terminal Run.
//
// Run with DEEPSEEK_API_KEY set (or a DEEPSEEK_API_KEY_FILE). The caller may
// project the user's ~/.bashrc assignment; this test never sources a shell file.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createHash, randomBytes } from 'node:crypto';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import {
  FILES_BETA, hasEndTurn, provisionProviderTestModel, spawnProduction, stopServer,
  waitForPort, waitForValue, waitForSessionEventReceipt, cleanupFixtureTree,
  retryableReadStatus,
} from './harness.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';
import { aggregateUsage, makeEvaluation, emitEvaluation } from './llm_eval_metrics.mjs';
import { TEMPLATE_IDS, TEMPLATE_SOURCE, templateScenario } from './fixtures/template_scenarios.mjs';
import { startTemplateMcp } from './fixtures/template_mcp_fixture.mjs';
import { assertTemplateResult } from './fixtures/template_scenario_assertions.mjs';

const BETAS = ['managed-agents-2026-04-01', FILES_BETA];
const PORT = Number(process.env.E2E_PORT);
const MODEL = process.env.DEEPSEEK_MODEL ?? 'deepseek-flash';
const timeoutMs = Number(process.env.AWAKEN_TEMPLATE_TIMEOUT_MS ?? 240_000);
const directory = path.resolve(process.env.AWAKEN_EVAL_ARTIFACT_DIR ?? `artifacts/template-live-${Date.now()}`);
const selected = process.env.AWAKEN_TEMPLATE_CASES?.split(',') ?? TEMPLATE_IDS;
const clientFor = (baseURL) => new Anthropic({ apiKey: 'e2e-local', baseURL, maxRetries: 0 }); // awaken-allow: secret (no-login test service placeholder)
assert.ok(selected.length > 0 && selected.every((id) => TEMPLATE_IDS.includes(id)), 'only known template cases may be selected');
assert.equal(new Set(selected).size, selected.length, 'no duplicate cases');

function toolsFor(scenario, denyWrites = false) {
  return [
    { type: 'agent_toolset_20260401', default_config: { enabled: false, permission_policy: { type: 'always_allow' } }, configs: ['read', 'write', 'bash'].map((name) => ({ name, enabled: true })) },
    ...Object.keys(scenario.tools).map((name) => ({
      type: 'mcp_toolset', mcp_server_name: name,
      default_config: { permission_policy: { type: denyWrites && name === 'slack' ? 'always_ask' : 'always_allow' } },
    })),
  ];
}

async function eventsList(client, id) {
  const rows = []; for await (const row of client.beta.sessions.events.list(id, { betas: BETAS })) rows.push(row); return rows;
}

async function runScenario(client, id, model, { denial = false } = {}) {
  const scenario = templateScenario(id, randomBytes(6).toString('hex'));
  const fixture = await startTemplateMcp(scenario);
  const caseId = denial ? `${id}-denied` : id;
  const caseDir = path.join(directory, caseId); fs.mkdirSync(caseDir, { recursive: true, mode: 0o700 });
  let session, deployment, agent, vault;
  const evidence = { template: id, variant: denial ? 'denied' : 'normal', model: MODEL, model_source: 'live-deepseek', business_services: 'local-mcp-fixtures', started_at: new Date().toISOString() };
  const started = Date.now();
  console.log(`START ${caseId}`);
  try {
    vault = await client.beta.vaults.create({ display_name: caseId, betas: BETAS });
    for (const server of fixture.servers) {
      await client.beta.vaults.credentials.create(vault.id, { auth: { type: 'static_bearer', mcp_server_url: server.url, token: fixture.token }, betas: BETAS });
    }
    const resources = [];
    for (const [filename, content] of Object.entries(scenario.files)) {
      const file = await client.beta.files.upload({ file: await toFile(Buffer.from(content), filename), betas: BETAS });
      resources.push({ type: 'file', file_id: file.id, mount_path: `/${filename}` });
    }
    agent = await client.beta.agents.create({
      name: `Template acceptance: ${caseId}`, model, system: scenario.system,
      mcp_servers: fixture.servers, tools: toolsFor(scenario, denial),
      metadata: { template: id, acceptance: 'real-model-local-mcp' }, betas: BETAS,
    });
    evidence.agent_id = agent.id;
    const task = scenario.task;
    let receipt;
    if (scenario.schedule) {
      deployment = await client.beta.deployments.create({
        name: caseId, agent: agent.id, environment_id: 'env_local', vault_ids: [vault.id],
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: task }] }],
        schedule: scenario.schedule, betas: BETAS,
      });
      assert.equal(deployment.schedule.timezone, scenario.schedule.timezone);
      assert.equal(deployment.schedule.expression, scenario.schedule.expression);
      await client.beta.deployments.pause(deployment.id, { betas: BETAS });
      const run = await client.beta.deployments.run(deployment.id, { betas: BETAS });
      evidence.deployment_id = deployment.id; evidence.deployment_run_id = run.id;
      const linked = await waitForValue(
        () => client.beta.deploymentRuns.retrieve(run.id, { betas: BETAS }),
        (value) => Boolean(value.session_id) || Boolean(value.error), 'deployment links a real Session', { timeoutMs: 30_000 },
      );
      assert.ok(linked.session_id, 'Deployment must create a Session');
      session = await client.beta.sessions.retrieve(linked.session_id, { betas: BETAS });
      const initial = await waitForValue(() => eventsList(client, session.id), (rows) => rows.some((e) => e.type === 'user.message'), 'initial scheduled message', { timeoutMs: 30_000 });
      receipt = initial.find((event) => event.type === 'user.message');
    } else {
      session = await client.beta.sessions.create({ agent: { type: 'agent', id: agent.id, version: agent.version }, environment_id: 'env_local', vault_ids: [vault.id], resources, betas: BETAS });
      receipt = (await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.message', content: [{ type: 'text', text: task }] }], betas: BETAS })).data[0];
    }
    evidence.session_id = session.id; evidence.receipt_id = receipt.id;
    let events;
    if (denial) {
      const awaiting = await waitForSessionEventReceipt(client, session.id, receipt.id, BETAS,
        ({ delta }) => delta.some((e) => e.type === 'session.status_idle' && e.stop_reason?.type === 'requires_action'), 'MCP write waits for authorization', { timeoutMs });
      assert.equal(fixture.state.effects.length, 0, 'no write before approval');
      const pending = awaiting.delta.filter((e) => e.type === 'session.status_idle').at(-1).stop_reason.event_ids;
      const tool = awaiting.events.find((e) => pending.includes(e.id) && e.type === 'agent.mcp_tool_use');
      assert.ok(tool, 'pending boundary is an actual MCP tool call');
      const denied = (await client.beta.sessions.events.send(session.id, { events: [{ type: 'user.tool_confirmation', tool_use_id: tool.id, result: 'deny' }], betas: BETAS })).data[0];
      evidence.denial_receipt_id = denied.id;
      ({ events } = await waitForSessionEventReceipt(client, session.id, denied.id, BETAS, ({ delta }) => hasEndTurn(delta), 'denied write settles without bypass', { timeoutMs }));
      assert.equal(fixture.state.effects.length, 0, 'denied write never executes');
    } else {
      ({ events } = await waitForSessionEventReceipt(client, session.id, receipt.id, BETAS, ({ delta }) => hasEndTurn(delta), `${id} committed end_turn`, { timeoutMs }));
      // O1 a completed research draft + explicit Outcome definition -> one
      // real Worker/Grader workflow and a satisfied terminal report. The
      // independent file oracle below must still pass; grading is not its
      // substitute. Infrastructure errors and iteration exhaustion fail O1.
      if (scenario.outcome) {
        const outcomeReceipt = (await client.beta.sessions.events.send(session.id, { events: [{
          type: 'user.define_outcome', description: scenario.outcome.description,
          rubric: { type: 'text', content: scenario.outcome.rubric },
          max_iterations: scenario.outcome.max_iterations,
        }], betas: BETAS })).data[0];
        evidence.outcome_receipt_id = outcomeReceipt.id;
        ({ events } = await waitForSessionEventReceipt(client, session.id, outcomeReceipt.id, BETAS,
          ({ delta }) => delta.some((e) => e.type === 'span.outcome_evaluation_end'),
          'real research Outcome grading commits', { timeoutMs }));
        const evaluated = await waitForValue(() => client.beta.sessions.retrieve(session.id, { betas: BETAS }),
          (s) => s.outcome_evaluations?.some((o) => o.completed_at), 'Outcome terminal readback', { timeoutMs });
        evidence.outcome = evaluated.outcome_evaluations;
        assert.equal(evaluated.outcome_evaluations.at(-1).result, 'satisfied', 'real Outcome must pass');
        events = await eventsList(client, session.id);
      }
      assert.ok(events.some((e) => e.type === 'agent.tool_use'), 'model must execute real sandbox tools');
      if (fixture.servers.length) assert.ok(events.some((e) => e.type === 'agent.mcp_tool_use'), 'model must execute real MCP calls');
      const listed = [];
      for await (const file of client.beta.files.list({ scope_id: session.id, betas: BETAS })) listed.push(file);
      const artifacts = {};
      evidence.output_files = [];
      for (const name of scenario.artifacts) {
        const file = listed.find((f) => path.posix.basename(f.filename ?? '') === name);
        assert.ok(file, `harvested artifact ${name} missing`);
        artifacts[name] = Buffer.from(await (await client.beta.files.download(file.id, { betas: BETAS })).arrayBuffer());
        evidence.output_files.push({ id: file.id, name, sha256: createHash('sha256').update(artifacts[name]).digest('hex') });
        fs.writeFileSync(path.join(caseDir, name), artifacts[name]);
      }
      evidence.result = assertTemplateResult(scenario, fixture.state, artifacts);
    }
    const fresh = clientFor(client.baseURL);
    const readback = await fresh.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(readback.agent.id, agent.id, 'fresh SDK readback preserves the exact Agent');
    evidence.usage = aggregateUsage(events);
    evidence.model_requests = events.filter((event) => event.type === 'span.model_request_end').length;
    // Tool errors can be meaningful evidence (the repro intentionally raises
    // TypeError). Retain them instead of hiding recoveries behind a green
    // final answer; the business oracle decides which effects are required.
    evidence.tool_errors = events.filter((event) =>
      ['agent.tool_result', 'agent.mcp_tool_result'].includes(event.type) && event.is_error,
    ).map((event) => ({ id: event.id, content: JSON.stringify(event.content).slice(0, 600) }));
    assert.ok(evidence.usage.observed && evidence.usage.input_tokens > 0, 'real model usage must be observable');
    fs.writeFileSync(path.join(caseDir, 'events.json'), JSON.stringify(events, null, 2));
    evidence.pass = true;
  } catch (error) {
    evidence.pass = false; evidence.failure = String(error.message ?? error).slice(0, 4000);
    if (session) {
      try { fs.writeFileSync(path.join(caseDir, 'events.json'), JSON.stringify(await eventsList(client, session.id), null, 2)); } catch { /* original failure remains */ }
    }
  } finally {
    evidence.elapsed_ms = Date.now() - started;
    evidence.mcp = fixture.state;
    fs.writeFileSync(path.join(caseDir, 'evidence.json'), JSON.stringify(evidence, null, 2));
    if (deployment) await client.beta.deployments.archive(deployment.id, { betas: BETAS }).catch(() => {});
    if (session && !evidence.pass) await client.beta.sessions.delete(session.id, { betas: BETAS }).catch(() => {});
    await fixture.close();
  }
  console.log(`${evidence.pass ? 'PASS' : 'FAIL'} ${caseId}${evidence.failure ? `: ${evidence.failure}` : ''}`);
  return evidence;
}

async function main() {
  const key = process.env.DEEPSEEK_API_KEY ?? (process.env.DEEPSEEK_API_KEY_FILE ? fs.readFileSync(process.env.DEEPSEEK_API_KEY_FILE, 'utf8').trim() : undefined);
  assert.ok(key, 'A real DEEPSEEK_API_KEY or DEEPSEEK_API_KEY_FILE is required; this suite never skips');
  fs.mkdirSync(directory, { recursive: true, mode: 0o700 });
  const data = path.join(directory, 'server-data');
  let server;
  const rows = [];
  const launch = () => spawnProduction(data, PORT, { stdout: 'inherit', fields: {
    sandbox_tier: 'namespace',
    log_filter: process.env.AWAKEN_TEMPLATE_LOG_FILTER ?? 'warn',
  } }, withoutAmbientProviderKeys(process.env));
  try {
    // File inputs require an enforcing namespace, not an advisory Workdir
    // mount. Reuse the shipped Namespace substrate and fail if unavailable.
    server = launch();
    await waitForPort(PORT, 30_000);
    const base = `http://127.0.0.1:${PORT}`;
    const response = await fetch(`${base}/v1/config/workspace-context`);
    assert.ok(response.ok); const workspace = response.headers.get('anthropic-workspace-id');
    assert.ok(workspace, 'real workspace authority returns its coordinate');
    console.log(`Provisioning real DeepSeek ${MODEL}`);
    const supplied = await provisionProviderTestModel(base, workspace, {
      id: 'template-deepseek', provider_id: 'deepseek', dialect: 'open_ai_chat',
      base_url: 'https://api.deepseek.com', secret: key, model_id: MODEL, timeout_secs: 180,
    });
    const client = clientFor(base);
    for (const id of selected) rows.push(await runScenario(client, id, supplied.model));
    if (selected.includes('support-agent')) rows.push(await runScenario(client, 'support-agent', supplied.model, { denial: true }));
    // Recovery table: P1 completed task+process replacement -> same Session and
    // artifact bytes, no new model request; P2 any missing/corrupt/replayed fact
    // -> fail its scenario. Only public SDK reads observe recovery.
    await stopServer(server); server = undefined;
    server = launch(); await waitForPort(PORT, 30_000);
    const recovered = clientFor(base);
    for (const row of rows.filter((r) => r.pass)) {
      try {
        // Restart-read decision table: R1 the HTTP listener is ready but its
        // rebuildable Agent/Environment projections are still converging ->
        // retry only the idempotent public read; R2 the projection converges ->
        // return the exact durable Session; R3 a non-retryable response or the
        // bound expires -> fail with that exact error. The canonical helper
        // owns retry classification and never repeats the completed model work.
        const retryReadError = (error) => retryableReadStatus(error) !== null;
        const persisted = await waitForValue(
          () => recovered.beta.sessions.retrieve(row.session_id, { betas: BETAS }),
          () => true,
          'Session root survives restart',
          { timeoutMs: 30_000, retryReadError },
        );
        assert.equal(persisted.agent.id, row.agent_id);
        const events = await waitForValue(
          () => eventsList(recovered, row.session_id),
          () => true,
          'Session events survive restart',
          { timeoutMs: 30_000, retryReadError },
        );
        assert.equal(events.filter((e) => e.type === 'span.model_request_end').length, row.model_requests, 'recovery must not repeat model work');
        for (const output of row.output_files ?? []) {
          const bytes = Buffer.from(await (await recovered.beta.files.download(output.id, { betas: BETAS })).arrayBuffer());
          assert.equal(createHash('sha256').update(bytes).digest('hex'), output.sha256, 'artifact survives restart unchanged');
        }
        row.recovery_pass = true;
      } catch (error) { row.pass = false; row.recovery_pass = false; row.failure = `restart: ${error.message}`; }
      const caseId = row.variant === 'denied' ? `${row.template}-denied` : row.template;
      fs.writeFileSync(path.join(directory, caseId, 'evidence.json'), JSON.stringify(row, null, 2));
      console.log(`${row.recovery_pass ? 'PASS' : 'FAIL'} restart ${caseId}`);
    }
    const passed = rows.filter((r) => r.pass).length;
    const evaluation = makeEvaluation({ suite: 'managed_template_scenarios_real', subject: 'nine-quickstart-jobs', backend: 'native', model: MODEL, sampleSize: rows.length,
      metrics: { pass_rate: passed / rows.length, cases_passed: passed, cases_total: rows.length },
      thresholds: { pass_rate: { operator: 'gte', value: 1 } },
      details: { source: TEMPLATE_SOURCE, selected_templates: selected, full_template_set: selected.length === TEMPLATE_IDS.length, model_source: 'real-deepseek', services: 'local-MCP-substitutes; no live SaaS claim', cases: rows.map(({ mcp, result, ...r }) => r) },
    });
    emitEvaluation(evaluation, { AWAKEN_EVAL_ARTIFACT_DIR: directory }, false);
    process.exitCode = evaluation.pass ? 0 : 1;
  } finally {
    if (server) await stopServer(server);
    // No reusable credential material or seal key belongs in result artifacts.
    // Preserve the canonical fixture tree only when an operator explicitly
    // requests post-failure inspection. Ordinary and CI runs retain the one
    // cleanup path shared by every outcome above.
    if (!process.env.AWAKEN_E2E_KEEP_TMP) cleanupFixtureTree(data);
  }
}
main().catch((error) => { console.error(`E2E FAIL: ${error.message}`); process.exitCode = 1; });
