// Config data plane end-to-end (slice A): author → validate → publish → run.
//
// Over the `config` server (an in-memory SQLite config store + `/v1/config/*`),
// we PUT a declarative agent config, validate it, publish it (compile → store
// publication → install into the live catalog), then create a managed session for
// that agent id and run one turn. The `config` server's model echoes the system
// prompt, so the reply proves the *published agent's own instructions* reached the
// run — i.e. the config→compile→install→execute loop works end to end via HTTP.
//
// Run: (from e2e/)  node managed_config_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass, waitForSessionEventReceipt } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38160);
const BETAS = ['managed-agents-2026-04-01'];

const AGENT = 'greeter';
const GREETING = 'HELLO-FROM-CONFIG';
// The config-plane agent endpoints parse the managed-shaped agent object
// (agent_config_from_managed): instructions live under `system`, the model under
// `model`, tool references under `tools`. Author the draft in that shape so the
// stored config truth carries the model/instructions the projection asserts below.
const agentConfig = {
  id: AGENT,
  name: 'Managed config greeter',
  description: 'Exercises the complete managed authoring projection.',
  system: GREETING,
  max_steps: 4,
  model: { id: 'config-model' },
  // Accept both managed tool reference shapes. Non-reference values are ignored
  // at the wire adapter instead of leaking into the domain config.
  tools: [{ id: 'bash' }, 7],
  plugins: [],
  plugin_config: { marker: 'managed-wire' },
  context_policy: { kind: 'keep_last', keep_last: 3 },
  tool_overrides: [{
    target: 'mcp__future__lookup',
    alias: 'lookup',
    description: 'Look up a future MCP value.',
    exposure: "on_demand",
  }],
  recovery_policies: {},
  metadata: { owner: 'e2e', ignored_non_string: 7 },
  mcp_servers: [],
  skills: [],
  multiagent: null,
};

async function main() {
  await withScenarioServer('config', 'instruction', PORT, async (baseUrl) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const json = async (method, path, body) => {
      const res = await fetch(`${baseUrl}${path}`, {
        method,
        headers: {
          'anthropic-beta': BETAS[0],
          ...(body === undefined ? {} : { 'content-type': 'application/json' }),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      return { status: res.status, body: await res.json().catch(() => ({})) };
    };

    // Author.
    const put = await json('PUT', `/v1/config/agents/${AGENT}`, agentConfig);
    assert.equal(put.status, 200, 'config stored');
    pass('agent config stored');

    // Agent-tool authority cause/effect graph: C1 the draft already contains a
    // valid config; C2 a replacement names a member outside the closed official
    // Agent toolset. Effects: E1 C2 returns 400 before normalization; E2 the
    // prior draft and generation remain byte-for-byte observable. Decision row
    // A1: C1+C2 => E1+E2. K/Constraint: Config Service reuses the Session-contract
    // validator and cannot preserve or silently drop a parallel tool identity.
    const beforeUnknownTool = await json('GET', `/v1/config/agents/${AGENT}`, undefined);
    assert.equal(beforeUnknownTool.status, 200, 'A1 baseline draft exists');
    const unknownToolPut = await json('PUT', `/v1/config/agents/${AGENT}`, {
      ...agentConfig,
      tools: [{
        type: 'agent_toolset_20260401',
        configs: [{ name: 'parallel_web_search' }],
      }],
    });
    assert.equal(unknownToolPut.status, 400, JSON.stringify(unknownToolPut.body));
    assert.match(unknownToolPut.body.error, /unknown agent tool `parallel_web_search`/u, 'A1/E1');
    assert.deepEqual(
      await json('GET', `/v1/config/agents/${AGENT}`, undefined),
      beforeUnknownTool,
      'A1/E2 rejected replacement has no draft or generation side effect',
    );
    pass('unknown Agent-tool config fails closed before Config draft mutation');

    // Validate (compile dry-run).
    const valid = await json('POST', `/v1/config/agents/${AGENT}/validate`, agentConfig);
    assert.equal(valid.status, 200, 'config validates');
    assert.equal(valid.body.valid, true, 'valid=true');
    pass('config validated (compiles)');

    // A bad config (unknown tool) fails validation. Validation is a QUERY (ADR-0053,
    // field-routed structured issues for the UI): the request succeeds with 200 and
    // reports `valid: false` + a structured issue, rather than a transport 400 — a
    // name absent from the catalog fails closed with UnknownTool (config_plane D3).
    const bad = await json('POST', `/v1/config/agents/${AGENT}/validate`, {
      id: AGENT,
      system: GREETING,
      tools: [{ name: 'no_such_tool' }],
    });
    assert.equal(bad.status, 200, 'validation is a query — 200 with the verdict');
    assert.equal(bad.body.valid, false, 'valid=false on unknown tool');
    assert.ok(Array.isArray(bad.body.issues) && bad.body.issues.length > 0, 'a structured issue is reported');
    pass('invalid config reported invalid (unknown tool)');

    // MCP servers, their execution policies, and Skill selectors are typed
    // authoring metadata compiled into one normalized, secret-free binding
    // section. Validate them on a separate Agent so this test covers the
    // configuration boundary without making the executable greeter dial an
    // external MCP endpoint.
    const bindingsAgent = 'binding-normalization';
    const bindingConfig = {
      name: bindingsAgent,
      system: 'Validate resource-like Agent bindings.',
      model: { id: 'config-model' },
      tools: [
        { type: 'mcp_toolset', mcp_server_name: 'docs' },
        { type: 'mcp_toolset', mcp_server_name: 'local' },
      ],
      mcp_servers: [
        { type: 'url', name: 'docs', url: 'https://mcp.example.invalid/v1' },
        { type: 'url', name: 'local', url: 'http://127.0.0.1:9/mcp' },
      ],
      skills: [
        { type: 'custom', skill_id: 'skill-a' },
        { type: 'custom', skill_id: 'skill-b' },
      ],
    };
    const bindingsValid = await json(
      'POST',
      `/v1/config/agents/${bindingsAgent}/validate`,
      bindingConfig,
    );
    assert.equal(bindingsValid.status, 200);
    assert.equal(bindingsValid.body.valid, true, JSON.stringify(bindingsValid.body));

    // Validation transport decision table: R1 malformed typed JSON or a broken
    // Managed server/toolset admission invariant -> 400/valid:false; R2 an
    // admitted binding with a later compile error -> 200/valid:false. Each row
    // keeps every unrelated server/toolset or Skill axis coherent, so its named
    // cause is the only possible failure authority. The config-plane unit test
    // owns the detailed compiler causes.
    const invalidBindingCases = [
      { value: { mcp_servers: ['not-an-object'], skills: [] }, status: 400 },
      {
        value: { mcp_servers: [{ type: 'url', url: 'https://mcp.example.invalid' }] },
        status: 400,
      },
      { value: { mcp_servers: [{ type: 'url', name: 'docs' }] }, status: 400 },
      {
        value: {
          mcp_servers: [{ type: 'url', name: 'docs', url: 'file:///tmp/mcp' }],
          tools: [{ type: 'mcp_toolset', mcp_server_name: 'docs' }],
        },
        status: 400,
      },
      {
        value: {
          mcp_servers: [
            { type: 'url', name: 'duplicate', url: 'https://one.example.invalid' },
            { type: 'url', name: 'duplicate', url: 'https://two.example.invalid' },
          ],
          tools: [{ type: 'mcp_toolset', mcp_server_name: 'duplicate' }],
        },
        status: 400,
      },
      { value: { mcp_servers: [], tools: [], skills: [7] }, status: 400 },
      {
        value: {
          mcp_servers: [],
          tools: [],
          skills: [{ type: 'custom', skill_id: '' }],
        },
        status: 200,
      },
      {
        value: {
          mcp_servers: [],
          tools: [],
          skills: [
            { type: 'custom', skill_id: 'skill-a' },
            { type: 'custom', skill_id: 'skill-a' },
          ],
        },
        status: 200,
      },
    ];
    for (const invalidBindings of invalidBindingCases) {
      const verdict = await json(
        'POST',
        `/v1/config/agents/${bindingsAgent}/validate`,
        { ...bindingConfig, ...invalidBindings.value },
      );
      assert.equal(verdict.status, invalidBindings.status, JSON.stringify(verdict.body));
      if (verdict.status === 200) {
        assert.equal(verdict.body.valid, false, JSON.stringify(verdict.body));
        assert.ok(verdict.body.issues[0].path === 'mcp_servers' || verdict.body.issues[0].path === 'skills');
      }
    }
    pass('Agent MCP/Skill bindings normalize once and malformed bindings fail closed');

    // Publish (compile → store publication → install).
    const published = await json('POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(published.status, 200, 'published');
    assert.ok(published.body.fingerprint, 'publication has a fingerprint');
    assert.equal(published.body.installed, true, 'installed into the live catalog');
    pass(`published: fingerprint ${published.body.fingerprint.slice(0, 12)}…`);

    const authored = await json('GET', `/v1/config/agents/${AGENT}`, undefined);
    assert.equal(authored.status, 200);
    assert.deepEqual(authored.body.tools, ['bash']);
    assert.deepEqual(authored.body.metadata, { owner: 'e2e' });
    assert.deepEqual(authored.body.context_policy, { kind: 'keep_last', keep_last: 3 });
    assert.equal(authored.body.tool_overrides[0].alias, 'lookup');

    // Retreat to projection: the published agent appears on `/v1/agents` as a
    // managed-wire projection of the config truth — model/system come from the
    // published config, though it was never created via the agents registry.
    const projected = await json('GET', `/v1/agents/${AGENT}`, undefined);
    assert.equal(
      projected.status,
      200,
      `published agent is retrievable via /v1/agents: ${JSON.stringify(projected.body)}`,
    );
    assert.equal(projected.body.id, AGENT);
    assert.equal(projected.body.model.id, 'config-model', 'model projected from config truth');
    assert.equal(projected.body.system, GREETING, 'system projected from config instructions');
    assert.deepEqual(
      projected.body.tools,
      [],
      'a server-executed builtin is not retyped as a client-executed custom tool',
    );
    pass('published config agent projects onto /v1/agents');

    // Run: a session for the published agent runs with its own instructions.
    const session = await client.beta.sessions.create({ agent: AGENT, environment_id: 'env_local', betas: BETAS });
    // C1=published Agent admits one exact User receipt; C2=its configured
    // instructions appear in the later Agent reply. E1=C2 is scoped after C1.
    // K: projection/listing is observation only. Decision P1 C1&&!C2=>retry;
    // P2 C1+C2=>assert the published instruction marker.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'P1 exact published-Agent User Event receipt');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      'P1 published Agent Run to commit its reply',
    );
    const assistant = events.filter((e) => e.type === 'agent.message');
    const text = JSON.stringify(assistant.map((m) => m.content));
    assert.ok(
      text.includes(GREETING),
      `run used the published agent's instructions (${GREETING}); events=${text}`,
    );
    pass('published agent ran with its own instructions');

    console.log('E2E PASS: config author→validate→publish→run loop via HTTP + TS SDK.');
  });
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
