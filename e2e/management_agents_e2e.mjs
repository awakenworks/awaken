// The public agent registry, driven by the official Anthropic TypeScript SDK
// (`client.beta.agents.*`): create / retrieve / update / list / archive + version
// history. Any wire-shape drift from the official `BetaManagedAgentsAgent` type
// surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_agents_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function runTurn(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  return drain(client.beta.sessions.events.list(sessionId, { betas: BETAS }));
}

const agentTexts = (events) => events
  .filter((event) => event.type === 'agent.message')
  .flatMap((event) => event.content ?? [])
  .map((block) => block.text ?? '');

async function json(baseUrl, method, route, body) {
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: {
      'anthropic-beta': BETAS[0],
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function main() {
  try {
    await withScenarioServer('management-agents', 'default', 38138, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const agent = await client.beta.agents.create({
        name: 'assistant',
        model: 'claude-opus-4-8',
        system: 'be helpful',
        metadata: { team: 'core' },
        betas: BETAS,
      });
      assert.equal(agent.type, 'agent');
      assert.ok(agent.id.startsWith('agent_'), `id: ${agent.id}`);
      assert.equal(agent.version, 1);
      assert.equal(agent.model.id, 'claude-opus-4-8', 'model string normalized to a ModelConfig');
      pass('beta.agents.create -> BetaManagedAgentsAgent (model normalized)');

      const got = await client.beta.agents.retrieve(agent.id, { betas: BETAS });
      assert.equal(got.id, agent.id);
      pass('beta.agents.retrieve -> BetaManagedAgentsAgent');

      const updated = await client.beta.agents.update(agent.id, {
        version: agent.version,
        name: 'assistant-2',
        system: 'be concise',
        betas: BETAS,
      });
      assert.equal(updated.version, 2);
      assert.equal(updated.name, 'assistant-2');
      pass('beta.agents.update -> version bumped to 2');

      const historical = await client.beta.agents.retrieve(agent.id, {
        version: 1,
        betas: BETAS,
      });
      assert.equal(historical.name, 'assistant', 'version query returns immutable revision 1');
      await assert.rejects(
        () => client.beta.agents.retrieve(agent.id, { version: 999, betas: BETAS }),
        (error) => error.status === 404,
      );

      // A stale version conflicts.
      await assert.rejects(
        () => client.beta.agents.update(agent.id, { version: 1, name: 'nope', betas: BETAS }),
        (err) => err.status === 409,
      );
      pass('beta.agents.update(stale version) -> 409');

      // [SDK:resources/beta/agents/versions.d.ts]
      // Causal graph:
      // create/update commits immutable revisions -> versions.list orders the
      // complete history -> PagePromise follows cursors; archive appends one
      // terminal revision; an unknown aggregate produces no fabricated history.
      //
      // Decision table:
      // | Rule | Agent | Mutations       | limit | Effect |
      // | V1   | exists| create + update | none  | exact revisions 1,2 |
      // | V2   | exists| create + update | 1     | SDK follows cursor; same 1,2 |
      // | V3   | exists| then archive    | 1     | immutable 1,2 + archived 3 |
      // | V4   | missing| none           | any   | 404; no history |
      const versions = await drain(client.beta.agents.versions.list(agent.id, { betas: BETAS }));
      assert.deepEqual(versions.map((v) => [v.version, v.name]), [
        [1, 'assistant'],
        [2, 'assistant-2'],
      ], 'V1: update appends and does not rewrite the original revision');
      const pagedVersions = await drain(client.beta.agents.versions.list(agent.id, {
        limit: 1,
        betas: BETAS,
      }));
      assert.deepEqual(
        pagedVersions.map((v) => v.version),
        [1, 2],
        'V2: official SDK PagePromise consumes every version page in order',
      );
      pass(`beta.agents.versions.list -> ${versions.length} immutable versions + cursor traversal`);

      const ids = (await drain(client.beta.agents.list({ betas: BETAS }))).map((a) => a.id);
      assert.ok(ids.includes(agent.id));
      pass('beta.agents.list -> PageCursor<BetaManagedAgentsAgent>');

      const archived = await client.beta.agents.archive(agent.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archived agent carries archived_at');
      const archivedVersions = await drain(client.beta.agents.versions.list(agent.id, {
        limit: 1,
        betas: BETAS,
      }));
      assert.deepEqual(archivedVersions.map((v) => v.version), [1, 2, 3], 'V3');
      assert.equal(archivedVersions[0].archived_at, null, 'V3 old revision remains live history');
      assert.equal(archivedVersions[1].archived_at, null, 'V3 update revision remains unchanged');
      assert.ok(archivedVersions[2].archived_at, 'V3 terminal revision records archive');
      pass('beta.agents.archive -> archived_at set');

      const activeOnly = await drain(client.beta.agents.list({ betas: BETAS }));
      assert.ok(!activeOnly.some((candidate) => candidate.id === agent.id));
      const withArchived = await drain(client.beta.agents.list({
        include_archived: true,
        betas: BETAS,
      }));
      assert.ok(withArchived.some((candidate) => candidate.id === agent.id));
      const exactCreated = await drain(client.beta.agents.list({
        'created_at[gte]': agent.created_at,
        'created_at[lte]': agent.created_at,
        include_archived: true,
        betas: BETAS,
      }));
      assert.ok(exactCreated.some((candidate) => candidate.id === agent.id));
      assert.deepEqual(await drain(client.beta.agents.list({
        'created_at[gte]': '9999-01-01T00:00:00Z',
        include_archived: true,
        betas: BETAS,
      })), []);
      assert.deepEqual(await drain(client.beta.agents.list({
        'created_at[lte]': '2000-01-01T00:00:00Z',
        include_archived: true,
        betas: BETAS,
      })), []);

      // [SDK:resources/beta/agents/agents.d.ts]
      // Cause/effect graph:
      // official tagged SDK unions -> Managed admission -> config normalization
      // -> Agent projection. Updates distinguish omission, null, and value;
      // unknown tags/fields fail before persistence.
      //
      // Decision table:
      // | composite input                         | result |
      // | model speed + bare/tagged effort        | exact revision + execution controls |
      // | URL MCP/prebuilt+custom skills/tools    | 200 + typed projection |
      // | unknown/misspelled union member         | 400, no Agent created  |
      // | MCP declaration/toolset not bijective   | 400, no revision       |
      // | duplicate/unknown toolset member        | 400, no revision       |
      // | update field omitted                    | preserve current value |
      // | nullable update field = null            | clear exact field/bag  |
      // | metadata value = null                   | delete only that key   |
      // | update version omitted                  | unconditional CAS write|
      // | matching/stale version + semantic no-op | same revision / 409     |
      // | same model + effort omitted             | preserve prior effort   |
      // | changed model + effort omitted          | reset model default     |
      // | MCP/Skill at/over documented boundary   | accept / reject atomic  |
      // | list archived/time partition            | filter before paging   |
      const rosterWorker = await client.beta.agents.create({
        name: 'roster-worker',
        model: 'claude-sonnet-5',
        betas: BETAS,
      });
      // Client-tool ownership decision table: C1 a client-executed descriptor
      // overlaps an enabled Agent toolset identity -> reject 400 with no Agent;
      // C2 every identity is unique -> preserve the complete inline contract.
      // Choosing one owner by insertion order would create two execution paths.
      const overlappingTool = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'overlapping-tool-owner',
        model: 'claude-sonnet-5',
        tools: [
          { type: 'agent_toolset_20260401' },
          {
            type: 'custom',
            name: 'bash',
            description: 'ambiguous client bash',
            input_schema: { type: 'object', properties: {} },
          },
        ],
      });
      assert.equal(overlappingTool.status, 400, 'C1 duplicate execution ownership');
      assert.match(overlappingTool.body.error.message, /duplicate tool identity "bash"/);
      assert.ok(
        !(await drain(client.beta.agents.list({ betas: BETAS })))
          .some((item) => item.name === 'overlapping-tool-owner'),
        'C1 rejection has no persisted Agent side effect',
      );
      const clientToolNames = ['client_bash', 'client_glob', 'client_read'];
      const rich = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'rich-agent',
        model: { id: 'claude-sonnet-5', speed: 'fast', effort: 'xhigh' },
        description: 'all mutable fields',
        system: 'rich system',
        metadata: { team: 'platform' },
        mcp_servers: [{ name: 'docs', type: 'url', url: 'https://example.invalid/mcp' }],
        skills: [
          { type: 'anthropic', skill_id: 'xlsx', version: '1' },
          { type: 'custom', skill_id: 'skill-a', version: '2' },
        ],
        tools: [
          {
            type: 'agent_toolset_20260401',
            configs: [{
              name: 'write',
              enabled: false,
              permission_policy: { type: 'always_allow' },
            }],
            default_config: { enabled: true, permission_policy: { type: 'always_ask' } },
          },
          {
            type: 'mcp_toolset',
            mcp_server_name: 'docs',
            default_config: { enabled: true, permission_policy: { type: 'always_ask' } },
          },
          ...clientToolNames.map((name) => ({
            type: 'custom',
            name,
            description: `${name} tool`,
            input_schema: { type: 'object', properties: {} },
          })),
        ],
        multiagent: { type: 'coordinator', agents: [rosterWorker.id] },
      });
      assert.equal(rich.status, 200, JSON.stringify(rich.body));
      assert.deepEqual(rich.body.model, {
        id: 'claude-sonnet-5',
        speed: 'fast',
        effort: { type: 'xhigh' },
      });
      assert.deepEqual(
        rich.body.tools,
        [
          {
            type: 'agent_toolset_20260401',
            configs: [{
              name: 'write',
              enabled: false,
              permission_policy: { type: 'always_allow' },
            }],
            default_config: { enabled: true, permission_policy: { type: 'always_ask' } },
          },
          {
            type: 'mcp_toolset',
            mcp_server_name: 'docs',
            configs: [],
            default_config: { enabled: true, permission_policy: { type: 'always_ask' } },
          },
          ...clientToolNames.map((name) => ({
            type: 'custom',
            name,
            description: `${name} tool`,
            input_schema: { type: 'object', properties: {} },
          })),
        ],
        'create preserves the complete client-tool behavior contract',
      );
      assert.deepEqual(rich.body.multiagent, {
        type: 'coordinator',
        agents: [{ type: 'agent', id: rosterWorker.id, version: 1 }],
      });
      assert.deepEqual(rich.body.skills, [
        { type: 'anthropic', skill_id: 'xlsx', version: '1' },
        { type: 'custom', skill_id: 'skill-a', version: '2' },
      ], 'prebuilt source and exact custom selector survive the Agent projection');

      // Multiagent cause/effect graph:
      // roster union + current Agent lifecycle/version -> one resolved immutable
      // roster snapshot. `self` resolves to the owner revision; short-form Agent
      // ids resolve once to current; malformed, duplicate, missing, archived, or
      // nested-coordinator targets fail before an Agent is persisted.
      //
      // | rule | roster cause | effect |
      // | R1 | one self | owner reference pinned to each owner revision |
      // | R2 | short id / exact old version | current pinned / exact preserved |
      // | R3 | empty / 21 / duplicate / version 0 | 400, no Agent |
      // | R4 | missing version/id or archived target | 400, no Agent |
      // | R5 | referenced coordinator | 400 depth-limit violation |
      const selfCoordinator = await client.beta.agents.create({
        name: 'self-coordinator',
        model: 'claude-sonnet-5',
        multiagent: { type: 'coordinator', agents: [{ type: 'self' }] },
        betas: BETAS,
      });
      assert.deepEqual(selfCoordinator.multiagent, {
        type: 'coordinator',
        agents: [{ type: 'agent', id: selfCoordinator.id, version: 1 }],
      }, 'R1 create resolves self to owner revision 1');
      const selfV2 = await client.beta.agents.update(selfCoordinator.id, {
        version: 1,
        name: 'self-coordinator-v2',
        betas: BETAS,
      });
      assert.deepEqual(selfV2.multiagent.agents, [
        { type: 'agent', id: selfCoordinator.id, version: 2 },
      ], 'R1 update snapshots the self copy at owner revision 2');
      assert.deepEqual((await client.beta.agents.retrieve(selfCoordinator.id, {
        version: 1,
        betas: BETAS,
      })).multiagent.agents, [
        { type: 'agent', id: selfCoordinator.id, version: 1 },
      ], 'R1 immutable history retains the former self snapshot');
      const explicitOwner = await json(baseUrl, 'POST', `/v1/agents/${selfCoordinator.id}`, {
        version: 2,
        multiagent: {
          type: 'coordinator',
          agents: [{ type: 'agent', id: selfCoordinator.id, version: 2 }],
        },
      });
      assert.equal(explicitOwner.status, 400, 'R3 an owner cycle must use the explicit self variant');

      const workerV2 = await client.beta.agents.update(rosterWorker.id, {
        version: 1,
        name: 'roster-worker-v2',
        betas: BETAS,
      });
      assert.equal(workerV2.version, 2);
      assert.deepEqual((await client.beta.agents.retrieve(rich.body.id, {
        betas: BETAS,
      })).multiagent.agents, [
        { type: 'agent', id: rosterWorker.id, version: 1 },
      ], 'R2 short-form target stays pinned after the target updates');
      const exactOld = await client.beta.agents.create({
        name: 'exact-old-coordinator',
        model: 'claude-sonnet-5',
        multiagent: {
          type: 'coordinator',
          agents: [{ type: 'agent', id: rosterWorker.id, version: 1 }],
        },
        betas: BETAS,
      });
      assert.deepEqual(exactOld.multiagent.agents, [
        { type: 'agent', id: rosterWorker.id, version: 1 },
      ], 'R2 exact historical target is preserved');

      // Runtime pin rules extend R2 from representation to behavior:
      // | rule | coordinator creation | worker current at Session start | child effect |
      // | R6 | before worker v2 | v2 | executes frozen v1 publication |
      // | R7 | after worker v2 | v2 | executes newly resolved v2 publication |
      const executionWorker = await client.beta.agents.create({
        name: 'execution-worker',
        model: 'management-agents',
        system: 'WORKER_REVISION_ONE',
        betas: BETAS,
      });
      const pinnedCoordinator = await client.beta.agents.create({
        name: 'pinned-coordinator',
        model: 'management-agents',
        system: 'coordinate',
        multiagent: { type: 'coordinator', agents: [executionWorker.id] },
        betas: BETAS,
      });
      const executionWorkerV2 = await client.beta.agents.update(executionWorker.id, {
        version: 1,
        system: 'WORKER_REVISION_TWO',
        betas: BETAS,
      });
      assert.equal(executionWorkerV2.version, 2);
      const latestCoordinator = await client.beta.agents.create({
        name: 'latest-coordinator',
        model: 'management-agents',
        system: 'coordinate',
        multiagent: { type: 'coordinator', agents: [executionWorker.id] },
        betas: BETAS,
      });
      for (const [rule, coordinator, expected, rejected] of [
        ['R6', pinnedCoordinator, 'WORKER_REVISION_ONE', 'WORKER_REVISION_TWO'],
        ['R7', latestCoordinator, 'WORKER_REVISION_TWO', 'WORKER_REVISION_ONE'],
      ]) {
        const session = await client.beta.sessions.create({
          agent: coordinator.id,
          environment_id: 'env_local',
          betas: BETAS,
        });
        const texts = agentTexts(await runTurn(client, session.id, `delegate to ${executionWorker.id}`));
        assert.ok(texts.some((text) => text.includes(expected)), `${rule} executes ${expected}: ${texts}`);
        assert.ok(!texts.some((text) => text.includes(rejected)), `${rule} must not drift to ${rejected}: ${texts}`);
      }

      const archivedTarget = await client.beta.agents.create({
        name: 'archived-roster-target', model: 'claude-sonnet-5', betas: BETAS,
      });
      await client.beta.agents.archive(archivedTarget.id, { betas: BETAS });
      const invalidRosters = [
        [],
        Array.from({ length: 21 }, (_, index) => `missing-${index}`),
        [rosterWorker.id, rosterWorker.id],
        [{ type: 'self' }, { type: 'self' }],
        [{ type: 'agent', id: rosterWorker.id, version: 0 }],
        [{ type: 'agent', id: rosterWorker.id, version: 999 }],
        ['agent_missing'],
        [archivedTarget.id],
        [selfCoordinator.id],
      ];
      for (const agents of invalidRosters) {
        const rejected = await json(baseUrl, 'POST', '/v1/agents', {
          name: 'invalid-roster',
          model: 'claude-sonnet-5',
          multiagent: { type: 'coordinator', agents },
        });
        assert.equal(rejected.status, 400, `R3/R4/R5: ${JSON.stringify(agents)} -> ${JSON.stringify(rejected.body)}`);
      }

      // Causes: matching or stale CAS revision, same/different model id, and
      // omitted effort. Constraints: stale rejection precedes no-op detection;
      // only an unchanged model inherits effort. Effects: B2/B3/B4/B5 preserve
      // or increment exactly one revision and never synthesize an effort value.
      const sameModel = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: rich.body.version,
        model: { id: 'claude-sonnet-5', speed: 'standard' },
      });
      assert.equal(sameModel.status, 200, JSON.stringify(sameModel.body));
      assert.equal(sameModel.body.version, rich.body.version + 1, 'B4');
      assert.deepEqual(sameModel.body.model, {
        id: 'claude-sonnet-5',
        speed: 'standard',
        effort: { type: 'xhigh' },
      }, 'same model preserves omitted effort');
      const matchingNoop = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: sameModel.body.version,
        model: { id: 'claude-sonnet-5', speed: 'standard' },
      });
      assert.equal(matchingNoop.body.version, sameModel.body.version, 'B3');
      const unconditionalNoop = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        name: rich.body.name,
      });
      assert.equal(unconditionalNoop.body.version, sameModel.body.version, 'B5');
      assert.equal((await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: rich.body.version,
        model: { id: 'claude-sonnet-5', speed: 'standard' },
      })).status, 409, 'B2: a stale semantic no-op still conflicts');
      assert.equal((await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: 0,
      })).status, 400, 'version has an inclusive minimum of one');
      const changedModel = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: sameModel.body.version,
        model: { id: 'claude-opus-5', speed: 'standard' },
      });
      assert.equal(changedModel.body.version, sameModel.body.version + 1, 'B4');
      assert.deepEqual(changedModel.body.model, {
        id: 'claude-opus-5', speed: 'standard',
      }, 'changed model resets omitted effort to its default');

      const richUpdated = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: changedModel.body.version,
        model: { id: 'claude-sonnet-5', speed: 'standard', effort: { type: 'low' } },
        description: 'replaced',
        system: 'replaced system',
        metadata: { team: 'runtime' },
        mcp_servers: [],
        skills: [],
        tools: [{
          type: 'custom',
          name: 'write',
          description: 'write tool',
          input_schema: { type: 'object', properties: {} },
        }],
        multiagent: null,
      });
      assert.equal(richUpdated.status, 200, JSON.stringify(richUpdated.body));
      assert.equal(richUpdated.body.description, 'replaced');
      assert.equal(richUpdated.body.system, 'replaced system');
      assert.deepEqual(richUpdated.body.model, {
        id: 'claude-sonnet-5',
        speed: 'standard',
        effort: { type: 'low' },
      });
      assert.deepEqual(richUpdated.body.metadata, { team: 'runtime' });
      assert.deepEqual(richUpdated.body.mcp_servers, []);
      assert.deepEqual(richUpdated.body.skills, []);
      assert.deepEqual(richUpdated.body.tools, [{
        type: 'custom',
        name: 'write',
        description: 'write tool',
        input_schema: { type: 'object', properties: {} },
      }], 'replacement preserves the complete client-tool behavior contract');
      assert.deepEqual(
        richUpdated.body.multiagent,
        null,
        'explicit null clears the coordinator topology',
      );

      const cleared = await client.beta.agents.update(rich.body.id, {
        description: null,
        system: null,
        metadata: { team: null, retained: 'yes' },
        mcp_servers: null,
        skills: null,
        tools: null,
        multiagent: null,
        betas: BETAS,
      });
      assert.equal(cleared.version, richUpdated.body.version + 1);
      assert.equal(cleared.description, null);
      assert.equal(cleared.system, null);
      assert.deepEqual(cleared.metadata, { retained: 'yes' });
      assert.deepEqual(cleared.mcp_servers, []);
      assert.deepEqual(cleared.skills, []);
      assert.deepEqual(cleared.tools, []);
      assert.equal(cleared.multiagent, null);
      assert.deepEqual(
        cleared.model,
        richUpdated.body.model,
        'omitting model preserves the exact resolved inference controls',
      );
      const richV2 = await client.beta.agents.retrieve(rich.body.id, {
        version: richUpdated.body.version,
        betas: BETAS,
      });
      assert.equal(richV2.description, 'replaced', 'later null clear does not rewrite history');
      assert.deepEqual(richV2.metadata, { team: 'runtime' });

      // Causes: MCP name/url and Skill selection are exactly at their inclusive
      // maxima. Constraints: one matching MCP toolset and unique non-empty Skill
      // ids. Effects: admission succeeds and the official DTO preserves every
      // value; the max+1/error partitions below reject without persistence.
      const maxMcpName = 'm'.repeat(255);
      const maxMcpUrl = `https://example.invalid/${'x'.repeat(2024)}`;
      assert.equal(maxMcpUrl.length, 2048);
      const boundaryMcpServers = [
        { name: maxMcpName, type: 'url', url: maxMcpUrl },
        ...Array.from({ length: 19 }, (_, i) => ({
          name: `mcp-${i}`, type: 'url', url: `https://mcp-${i}.example.invalid`,
        })),
      ];
      const boundaryAgent = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'boundary-agent',
        model: 'claude-sonnet-5',
        mcp_servers: boundaryMcpServers,
        tools: boundaryMcpServers.map(({ name }) => ({
          type: 'mcp_toolset', mcp_server_name: name,
        })),
        skills: Array.from({ length: 500 }, (_, i) => ({
          type: 'custom', skill_id: `skill-boundary-${i}`,
        })),
      });
      assert.equal(boundaryAgent.status, 200, JSON.stringify(boundaryAgent.body));
      assert.equal(boundaryAgent.body.mcp_servers.length, 20);
      assert.equal(boundaryAgent.body.mcp_servers[0].name.length, 255);
      assert.equal(boundaryAgent.body.mcp_servers[0].url.length, 2048);
      assert.equal(boundaryAgent.body.skills.length, 500);

      const countBeforeRejectedCreates = (await drain(client.beta.agents.list({
        include_archived: true,
        betas: BETAS,
      }))).length;
      for (const [name, invalid] of [
        ['invalid-skill-tag', { skills: [{ type: 'mystery', skill_id: 'skill-a' }] }],
        ['invalid-mcp-field', { mcp_servers: [{ type: 'url', name: 'docs', uri: 'https://wrong-field.test' }] }],
        ['invalid-toolset-field', { tools: [{ type: 'mcp_toolset', mcp_server: 'docs' }] }],
        ['unpaired-mcp-server', {
          mcp_servers: [{ name: 'docs', type: 'url', url: 'https://example.invalid/mcp' }],
        }],
        ['undeclared-mcp-toolset', {
          tools: [{ type: 'mcp_toolset', mcp_server_name: 'docs' }],
        }],
        ['duplicate-agent-toolset', {
          tools: [
            { type: 'agent_toolset_20260401' },
            { type: 'agent_toolset_20260401' },
          ],
        }],
        ['unknown-agent-tool', {
          tools: [{
            type: 'agent_toolset_20260401',
            configs: [{ name: 'not_a_tool', enabled: true }],
          }],
        }],
        ['empty-mcp-name', {
          mcp_servers: [{ name: '', type: 'url', url: 'https://example.invalid/mcp' }],
          tools: [{ type: 'mcp_toolset', mcp_server_name: '' }],
        }],
        ['long-mcp-name', {
          mcp_servers: [{ name: 'm'.repeat(256), type: 'url', url: 'https://example.invalid/mcp' }],
          tools: [{ type: 'mcp_toolset', mcp_server_name: 'm'.repeat(256) }],
        }],
        ['invalid-mcp-url', {
          mcp_servers: [{ name: 'docs', type: 'url', url: 'file:///tmp/mcp.sock' }],
          tools: [{ type: 'mcp_toolset', mcp_server_name: 'docs' }],
        }],
        ['long-mcp-url', {
          mcp_servers: [{ name: 'docs', type: 'url', url: `https://example.invalid/${'x'.repeat(2048)}` }],
          tools: [{ type: 'mcp_toolset', mcp_server_name: 'docs' }],
        }],
        ['empty-skill-id', { skills: [{ type: 'custom', skill_id: '' }] }],
        ['unknown-anthropic-skill', {
          skills: [{ type: 'anthropic', skill_id: 'unknown', version: 'latest' }],
        }],
        ['unavailable-anthropic-version', {
          skills: [{ type: 'anthropic', skill_id: 'xlsx', version: '2' }],
        }],
        ['zero-custom-skill-version', {
          skills: [{ type: 'custom', skill_id: 'skill-a', version: '0' }],
        }],
        ['non-numeric-custom-skill-version', {
          skills: [{ type: 'custom', skill_id: 'skill-a', version: 'current' }],
        }],
        ['duplicate-skill-id', { skills: [
          { type: 'custom', skill_id: 'skill-a' },
          { type: 'custom', skill_id: 'skill-a' },
        ] }],
        ['too-many-skills', { skills: Array.from({ length: 501 }, (_, i) => ({
          type: 'custom', skill_id: `skill-${i}`,
        })) }],
        ['too-many-mcp-servers', {
          mcp_servers: Array.from({ length: 21 }, (_, i) => ({
            name: `mcp-${i}`, type: 'url', url: `https://mcp-${i}.example.invalid`,
          })),
          tools: Array.from({ length: 21 }, (_, i) => ({
            type: 'mcp_toolset', mcp_server_name: `mcp-${i}`,
          })),
        }],
      ]) {
        const rejected = await json(baseUrl, 'POST', '/v1/agents', {
          name,
          model: 'claude-sonnet-5',
          ...invalid,
        });
        assert.equal(rejected.status, 400, JSON.stringify(rejected.body));
      }
      assert.equal(
        (await drain(client.beta.agents.list({ include_archived: true, betas: BETAS }))).length,
        countBeforeRejectedCreates,
        'rejected admission has no Agent persistence side effect',
      );

      const rejectedRevision = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: cleared.version,
        mcp_servers: [{ name: 'docs', type: 'url', url: 'https://example.invalid/mcp' }],
        tools: [],
      });
      assert.equal(rejectedRevision.status, 400, JSON.stringify(rejectedRevision.body));
      assert.equal(
        (await client.beta.agents.retrieve(rich.body.id, { betas: BETAS })).version,
        cleared.version,
        'rejected update creates no revision',
      );

      // Cause/effect graph:
      // Published --disable--> Disabled removes only the current executable
      // pointer; a repeated disable is idempotent and mutations fail closed.
      // Disabled --archive--> Archived appends one terminal revision.
      //
      // Decision table:
      // | L1 | Published + disable | Disabled; version +1 |
      // | L2 | Disabled + disable  | same version          |
      // | L3 | Disabled + update   | 400                   |
      // | L4 | Disabled + archive  | Archived; version +1  |
      const preDisableSession = await client.beta.sessions.create({
        agent: rich.body.id,
        betas: BETAS,
      });
      const richDisabled = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/disable`);
      assert.equal(richDisabled.status, 200, 'L1');
      assert.equal(richDisabled.body.status, 'disabled', 'L1');
      assert.ok(richDisabled.body.disabled_at, 'L1');
      const disabledAgain = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/disable`);
      assert.equal(disabledAgain.body.version, richDisabled.body.version, 'L2');
      await assert.rejects(
        () => client.beta.sessions.create({
          agent: rich.body.id,
          betas: BETAS,
        }),
        (error) => error.status === 400 && error.message.includes('cannot start a new session'),
        'L1: Disabled Agent is rejected before a new Session is admitted',
      );
      await assert.rejects(
        () => client.beta.sessions.events.send(preDisableSession.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'must not start' }] }],
          betas: BETAS,
        }),
        (error) => error.status === 400 && error.message.includes('cannot admit a new event'),
        'L1: an existing Session cannot admit a new Run after disable',
      );
      assert.equal(
        (await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
          version: richDisabled.body.version,
          name: 'must-not-update-disabled',
        })).status,
        400,
        'L3',
      );

      const richArchived = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/archive`);
      assert.equal(richArchived.status, 200);
      assert.equal(richArchived.body.status, 'archived', 'L4');
      assert.equal(richArchived.body.disabled_at, null, 'L4');
      assert.equal(richArchived.body.version, richDisabled.body.version + 1, 'L4');
      const archivedAgain = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/archive`);
      assert.equal(archivedAgain.status, 200);
      assert.equal(archivedAgain.body.version, richArchived.body.version);
      assert.equal(
        (await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
          version: richArchived.body.version,
          name: 'must-not-update',
        })).status,
        400,
      );

      for (const [method, route, body] of [
        ['GET', '/v1/agents/agent_missing', undefined],
        ['POST', '/v1/agents/agent_missing', { version: 1, name: 'missing' }],
        ['POST', '/v1/agents/agent_missing/disable', undefined],
        ['POST', '/v1/agents/agent_missing/archive', undefined],
        ['GET', '/v1/agents/agent_missing/versions', undefined],
      ]) {
        assert.equal((await json(baseUrl, method, route, body)).status, 404, route);
      }
      await assert.rejects(
        () => drain(client.beta.agents.versions.list('agent_missing', {
          limit: 1,
          betas: BETAS,
        })),
        (error) => error.status === 404,
        'V4',
      );

      const firstPage = await json(baseUrl, 'GET', '/v1/agents?limit=1&include_archived=true');
      assert.equal(firstPage.status, 200);
      assert.equal(firstPage.body.data.length, 1);
      assert.equal(firstPage.body.has_more, true);
      const secondPage = await json(
        baseUrl,
        'GET',
        `/v1/agents?limit=10&include_archived=true&page=${encodeURIComponent(firstPage.body.next_page)}`,
      );
      assert.equal(secondPage.status, 200);
      assert.ok(secondPage.body.data.length >= 1);
      pass('Agent rich projection, terminal fence, missing-id errors, and pagination');
    });

    console.log('E2E PASS: the agent registry round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
