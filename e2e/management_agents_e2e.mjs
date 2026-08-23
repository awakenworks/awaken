// The public agent registry, driven by the official Anthropic TypeScript SDK
// (`client.beta.agents.*`): create / retrieve / update / list / archive + version
// history. Any wire-shape drift from the official `BetaManagedAgentsAgent` type
// surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_agents_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass, waitForSessionEventReceipt } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function runTurn(client, sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  // Registry turn rule T0: C1=exact receipt; C2=coordinated work reaches a
  // post-receipt idle; E1=full committed history. Constraint: archived-agent
  // rejections remain synchronous and no older idle qualifies. C1&&!C2=>observe;
  // C1+C2=>E1; terminated=>fail immediately.
  const { events } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receipt.data[0]?.id,
    BETAS,
    async ({ delta }) => {
    const session = await client.beta.sessions.retrieve(sessionId, { betas: BETAS });
    if (session.status === 'terminated') throw new Error(`Session ${sessionId} terminated during coordination`);
      return session.status === 'idle'
        && delta.some((event) => event.type === 'session.status_idle');
    },
    `Session ${sessionId} did not settle after coordinated child work`,
    { timeoutMs: 60_000, pollMs: 200 },
  );
  return events;
}

const agentTexts = (events) => events
  .filter((event) => event.type === 'agent.message')
  .flatMap((event) => event.content ?? [])
  .map((block) => block.text ?? '');

const childReplyTexts = (events) => events
  .filter((event) => event.type === 'agent.thread_message_received')
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
      // | `us` + non-attesting Host candidate     | 400 before Agent persistence |
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
      const unsupportedGeo = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'unsupported-host-geo',
        model: { id: 'claude-sonnet-5', inference_geo: 'us' },
      });
      assert.equal(unsupportedGeo.status, 400, 'G1 host candidate cannot attest US processing');
      assert.match(unsupportedGeo.body.error.message, /cannot prove inference_geo `us`/);
      assert.ok(
        !(await drain(client.beta.agents.list({ betas: BETAS })))
          .some((item) => item.name === 'unsupported-host-geo'),
        'G1 geo rejection has no persisted Agent side effect',
      );
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
      // Web-tool configuration cause/effect graph: the discriminated input
      // config is validated once, normalized beside its ToolPolicyOverride,
      // persisted in the Agent revision, and projected with required output
      // `type`. Invalid combinations stop before repository mutation.
      //
      // Decision table:
      // | Rule | tool/type | domains | specific setting | effect |
      // | W1 | web_fetch/type omitted | allow only | positive max | persist + typed output |
      // | W2 | web_search/matching type | block only | approximate location | persist + typed output |
      // | W3 | either | allow + block | any | 400, no Agent |
      // | W4 | non-Web or mismatched type | Web field | any | 400, no Agent |
      // | W5 | web field boundary | empty/bad domain or bad country | 400, no Agent |
      // | W6 | web_fetch | no domains | zero max | persist exact zero cap |
      // | W7 | fetch location / search max | any | wrong tool-specific field | 400, no Agent |
      // K/Constraint: admission is atomic; a rejected config creates no Agent
      // and therefore cannot leave a revision or executable tool path behind.
      const configuredWeb = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'configured-web-tools',
        model: 'claude-sonnet-5',
        tools: [{
          type: 'agent_toolset_20260401',
          configs: [{
            name: 'web_fetch',
            allowed_domains: ['docs.example.com'],
            max_content_tokens: 4096,
          }, {
            name: 'web_search',
            type: 'web_search',
            blocked_domains: ['ads.example.com/tracker'],
            user_location: {
              type: 'approximate', city: 'Shanghai', country: 'CN',
              region: 'Shanghai', timezone: 'Asia/Shanghai',
            },
          }],
        }],
      });
      assert.equal(configuredWeb.status, 200, JSON.stringify(configuredWeb.body));
      assert.deepEqual(configuredWeb.body.tools[0].configs, [{
        name: 'web_fetch',
        type: 'web_fetch',
        enabled: true,
        permission_policy: { type: 'always_allow' },
        allowed_domains: ['docs.example.com'],
        max_content_tokens: 4096,
      }, {
        name: 'web_search',
        type: 'web_search',
        enabled: true,
        permission_policy: { type: 'always_allow' },
        blocked_domains: ['ads.example.com/tracker'],
        user_location: {
          type: 'approximate', city: 'Shanghai', country: 'CN',
          region: 'Shanghai', timezone: 'Asia/Shanghai',
        },
      }], 'W1/W2 normalized settings survive the durable projection');
      const gotConfiguredWeb = await client.beta.agents.retrieve(configuredWeb.body.id, {
        betas: BETAS,
      });
      assert.deepEqual(gotConfiguredWeb.tools, configuredWeb.body.tools, 'W1/W2 retrieve is exact');
      const zeroCapWeb = await json(baseUrl, 'POST', `/v1/agents/${configuredWeb.body.id}`, {
        version: 1,
        tools: [{
          type: 'agent_toolset_20260401',
          configs: [{ name: 'web_fetch', max_content_tokens: 0 }],
        }],
      });
      assert.equal(zeroCapWeb.status, 200, JSON.stringify(zeroCapWeb.body));
      assert.equal(zeroCapWeb.body.tools[0].configs[0].max_content_tokens, 0, 'W6');

      const invalidWebConfigs = [
        ['both-domain-modes', { name: 'web_fetch', allowed_domains: ['a.test'], blocked_domains: ['b.test'] }],
        ['mismatched-type', { name: 'web_fetch', type: 'web_search' }],
        ['null-type', { name: 'web_fetch', type: null }],
        ['web-field-on-bash', { name: 'bash', allowed_domains: ['a.test'] }],
        ['empty-domains', { name: 'web_search', allowed_domains: [] }],
        ['null-domains', { name: 'web_search', allowed_domains: null }],
        ['bad-domain', { name: 'web_fetch', allowed_domains: ['https://a.test'] }],
        ['bad-country', { name: 'web_search', user_location: { type: 'approximate', country: 'cn' } }],
        ['web-fetch-location', {
          name: 'web_fetch',
          user_location: { type: 'approximate', country: 'CN' },
        }],
        ['web-search-content-cap', { name: 'web_search', max_content_tokens: 1 }],
      ];
      for (const [name, config] of invalidWebConfigs) {
        const rejected = await json(baseUrl, 'POST', '/v1/agents', {
          name: `invalid-web-${name}`,
          model: 'claude-sonnet-5',
          tools: [{ type: 'agent_toolset_20260401', configs: [config] }],
        });
        assert.equal(rejected.status, 400, `${name}: ${JSON.stringify(rejected.body)}`);
      }
      const agentNamesAfterInvalidWeb = (await drain(client.beta.agents.list({ betas: BETAS })))
        .map((agent) => agent.name);
      for (const [name] of invalidWebConfigs) {
        assert.ok(!agentNamesAfterInvalidWeb.includes(`invalid-web-${name}`), `W3-W5/W7 ${name}`);
      }
      pass('Agent Web tool configuration decision table');
      const clientToolNames = ['client_bash', 'client_glob', 'client_read'];
      const rich = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'rich-agent',
        model: {
          id: 'claude-sonnet-5', speed: 'fast', effort: 'xhigh',
        },
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
              type: 'write',
              enabled: false,
              permission_policy: { type: 'always_allow' },
            }, {
              name: 'web_search',
              type: 'web_search',
              enabled: false,
              permission_policy: { type: 'always_ask' },
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
        multiagent: {
          type: 'coordinator',
          agents: [{ type: 'advisor', model: 'claude-opus-5' }, rosterWorker.id],
        },
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
              type: 'write',
              enabled: false,
              permission_policy: { type: 'always_allow' },
            }, {
              name: 'web_search',
              type: 'web_search',
              enabled: false,
              permission_policy: { type: 'always_ask' },
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
      // Advisor ordering cause/effect table: C1 submit advisor before an ordinary
      // child; C2 project create/list/retrieve/update. E1 the authoritative
      // authoring order may stay C1; E2 every Managed wire response preserves
      // ordinary order and places the advisor last. One repository projector
      // owns all four surfaces.
      const projectedRichRoster = {
        type: 'coordinator',
        agents: [
          { type: 'agent', id: rosterWorker.id, version: 1 },
          { type: 'advisor', model: 'claude-opus-5' },
        ],
      };
      assert.deepEqual(rich.body.multiagent, projectedRichRoster, 'C1/C2 create -> E2');
      const listedRich = (await drain(client.beta.agents.list({ betas: BETAS })))
        .find((agent) => agent.id === rich.body.id);
      assert.deepEqual(listedRich?.multiagent, projectedRichRoster, 'C1/C2 list -> E2');
      assert.deepEqual(rich.body.skills, [
        { type: 'anthropic', skill_id: 'xlsx', version: '1' },
        { type: 'custom', skill_id: 'skill-a', version: '2' },
      ], 'prebuilt source and exact custom selector survive the Agent projection');

      // Multiagent cause/effect graph:
      // roster union + current Agent lifecycle/version -> one resolved immutable
      // roster snapshot. `self` resolves to the owner revision; short-form Agent
      // ids resolve once to current; malformed, duplicate, missing, archived, or
      // nested-coordinator targets fail before an Agent is persisted. Exact
      // inference geo candidate proof and executor/advisor pairing are validated at that same
      // publication boundary; the advisor is projected last and is not an Agent.
      //
      // | rule | roster cause | effect |
      // | R1 | one self | owner reference pinned to each owner revision |
      // | R2 | short id / exact old version | current pinned / exact preserved |
      // | R3 | empty / 21 / duplicate / version 0 | 400, no Agent |
      // | R4 | missing version/id or archived target | 400, no Agent |
      // | R5 | referenced coordinator | 400 depth-limit violation |
      // | R8 | candidate cannot attest requested US geo | 400, no Agent |
      // | R9 | unsupported executor/advisor model pair | 400, no Agent |
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
      })).multiagent, projectedRichRoster, 'R2/C2 retrieve stays pinned and advisor-last');
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

      // Runtime pin rules extend R2 from representation to asynchronous Managed
      // behavior. Cause graph: coordinator publication time freezes the roster
      // revision -> list_agents exposes that roster -> send_to_agent admits a
      // child Thread -> the child reply reflects only the frozen publication.
      // The send receipt must not synchronously contain the child payload.
      //
      // | rule | coordinator creation | current worker | child Thread effect |
      // | R6 | before worker v2 | v2 | reply executes frozen v1 publication |
      // | R7 | after worker v2 | v2 | reply executes resolved v2 publication |
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
        const events = await runTurn(client, session.id, `delegate to ${executionWorker.id}`);
        const coordinatorTexts = agentTexts(events);
        const replies = childReplyTexts(events);
        assert.ok(
          coordinatorTexts.some((text) => text.includes('coordination accepted:')),
          `${rule} coordinator ends on a send receipt: ${coordinatorTexts}`,
        );
        assert.ok(
          coordinatorTexts.some((text) => text === 'coordination completed from child report'),
          `${rule} later child report ends without another send: ${coordinatorTexts}`,
        );
        assert.ok(
          !coordinatorTexts.some((text) => text.includes('WORKER_REVISION_')),
          `${rule} receipt is not a synchronous child result: ${coordinatorTexts}`,
        );
        assert.ok(replies.some((text) => text.includes(expected)), `${rule} executes ${expected}: ${replies}`);
        assert.ok(!replies.some((text) => text.includes(rejected)), `${rule} must not drift to ${rejected}: ${replies}`);
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
      assert.equal(unsupportedGeo.status, 400, 'R8 host publication cannot invent a geo proof');
      const advisorMismatch = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'advisor-mismatch',
        model: 'claude-opus-5',
        multiagent: {
          type: 'coordinator',
          agents: [{ type: 'advisor', model: 'claude-opus-4-8' }],
        },
      });
      assert.equal(advisorMismatch.status, 400, 'R9 unsupported advisor pair is atomic');

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
      assert.deepEqual(sameModel.body.multiagent, projectedRichRoster, 'C1/C2 update -> E2');
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
        skills: Array.from({ length: 20 }, (_, i) => ({
          type: 'custom', skill_id: `skill-boundary-${i}`,
        })),
      });
      assert.equal(boundaryAgent.status, 200, JSON.stringify(boundaryAgent.body));
      assert.equal(boundaryAgent.body.mcp_servers.length, 20);
      assert.equal(boundaryAgent.body.mcp_servers[0].name.length, 255);
      assert.equal(boundaryAgent.body.mcp_servers[0].url.length, 2048);
      assert.equal(boundaryAgent.body.skills.length, 20);

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
        ['too-many-skills', { skills: Array.from({ length: 21 }, (_, i) => ({
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

      // Cause/effect graph: Published --archive--> Archived removes the
      // current executable pointer, retains immutable history, and prevents
      // both new Sessions and new Runs in an existing Session. The historical
      // Awaken-only disable/status fields are deliberately absent from the
      // official Agent DTO and route surface.
      //
      // Decision table:
      // | L1 | Published + private disable route | 404; no mutation       |
      // | L2 | Published + archive               | version +1; archived  |
      // | L3 | Archived + archive                | same version          |
      // | L4 | Archived + update/start/run       | fail closed           |
      const preArchiveSession = await client.beta.sessions.create({
        agent: rich.body.id,
        environment_id: 'env_local',
        betas: BETAS,
      });
      const privateDisable = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/disable`);
      assert.equal(privateDisable.status, 404, 'L1');

      const richArchived = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/archive`);
      assert.equal(richArchived.status, 200, 'L2');
      assert.ok(richArchived.body.archived_at, 'L2');
      assert.equal(richArchived.body.version, cleared.version + 1, 'L2');
      assert.equal(richArchived.body.status, undefined, 'L2: no private status field');
      assert.equal(richArchived.body.disabled_at, undefined, 'L2: no private disabled_at field');
      const archivedAgain = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/archive`);
      assert.equal(archivedAgain.status, 200, 'L3');
      assert.equal(archivedAgain.body.version, richArchived.body.version, 'L3');
      await assert.rejects(
        () => client.beta.sessions.create({
          agent: rich.body.id,
          environment_id: 'env_local',
          betas: BETAS,
        }),
        (error) => error.status === 400 && error.message.includes('cannot start a new session'),
        'L4: Archived Agent is rejected before a new Session is admitted',
      );
      await assert.rejects(
        () => client.beta.sessions.events.send(preArchiveSession.id, {
          events: [{ type: 'user.message', content: [{ type: 'text', text: 'must not start' }] }],
          betas: BETAS,
        }),
        (error) => error.status === 400 && error.message.includes('cannot admit a new event'),
        'L4: an existing Session cannot admit a new Run after archive',
      );
      assert.equal(
        (await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
          version: richArchived.body.version,
          name: 'must-not-update-archived',
        })).status,
        400,
        'L4',
      );

      for (const [method, route, body] of [
        ['GET', '/v1/agents/agent_missing', undefined],
        ['POST', '/v1/agents/agent_missing', { version: 1, name: 'missing' }],
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
      assert.equal(typeof firstPage.body.next_page, 'string');
      assert.equal(firstPage.body.has_more, undefined, 'PageCursor has no Page-only has_more field');
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
