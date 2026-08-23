// Full Managed-Agents lifecycle e2e, driven end-to-end by the official Anthropic
// TypeScript SDK against awaken-server in `management` mode — the one build
// that wires the whole control plane (environments, the /v1/agents registry,
// vaults + MCP, memory stores, files, sessions + resources) over a deterministic
// executor (`McpToolModel`: "add a b" -> real ext-mcp calc.add, else echo).
//
// The arc the user asked for, in one server:
//   1. CREATE resources: environment, vault + MCP credential, file, memory store
//      (+ seed), and an agent in the registry (declaring the MCP server).
//   2. CREATE a session that ASSOCIATES them: agent id + environment_id + vault_ids
//      + mcp_servers + one exact create-time resource snapshot (file, memory_store,
//      github_repository).
//   3. RUN it: an MCP-tool Run ("add 2 3" -> mcp__calc__add -> "result: 5") and a
//      plain echo Run.
//   4. CHECK existing resources (env / agent / memory / file / attached resources)
//      and PRODUCED effects (tool_use + tool_result events, and the vault-materialized
//      bearer the MCP fixture saw on the wire).
//
// Run: (from e2e/)  npm install && node managed_full_lifecycle_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withScenarioServer } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_BETAS = ['agent-memory-2026-07-22'];
const CALC_TOKEN = 'calc-bearer-token-full-e2e'; // awaken-allow: secret
const FILE_MARK = 'FILE_MARK_5150'; // awaken-allow: secret
const MEM_MARK = 'MEM_MARK_2718'; // awaken-allow: secret
const PORT = Number(process.env.E2E_PORT ?? 38195);

const calcToolset = () => ({
  type: 'mcp_toolset',
  mcp_server_name: 'calc',
  default_config: {
    enabled: true,
    permission_policy: { type: 'always_allow' },
  },
});

async function drain(pageIter) {
  const out = [];
  for await (const item of pageIter) out.push(item);
  return out;
}

async function listEvents(client, sid) {
  return drain(client.beta.sessions.events.list(sid, { betas: BETAS }));
}

async function send(client, sid, text) {
  return client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function committedRunEvents(client, sid, receipt, terminalEffect, description) {
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'full-lifecycle Run returns its exact receipt');
  const { events } = await waitForSessionEventReceipt(
    client,
    sid,
    acceptedId,
    BETAS,
    ({ delta }) => terminalEffect(delta)
      && delta.some((event) => event.type === 'session.status_idle'),
    description,
  );
  return events;
}

const agentMessages = (events) =>
  events.filter((e) => e.type === 'agent.message').flatMap((e) => (e.content ?? []).map((c) => c.text ?? ''));

async function main() {
  // Cause/effect graph: C1=Control resources and typed Agent policy are valid;
  // C2=Local receives read-only mounts it cannot enforce after durable admission;
  // C3=an unmounted Session
  // binds the same Agent/vault; C4=the model requests MCP calc.add; C5=a later
  // plain User message reuses the Session. Effects: E1=resources freeze/read back;
  // E2=the exact receipt remains retained/unprocessed with no execution effect;
  // E3=the MCP result and vault bearer commit;
  // E4=the later Run echoes. Decision rules: R1 C1 => E1; R2 C1 && C2 => E2;
  // R3 C1 && C3 && C4 => E3; R4 R3 && C5 => E4.
  // Constraints/invariants: accepted Session/Event roots remain the only
  // durable authorities; a capability failure retains the exact command for
  // retry without fabricating model, tool, terminal, or deletion effects.
  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withScenarioServer('management', 'mcp', PORT, async (baseUrl, upstream) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // ── 1. CREATE RESOURCES ────────────────────────────────────────────────

      // environment
      const env = await client.beta.environments.create({
        name: `env-full-${PORT}`,
        config: { type: 'cloud', networking: { type: 'unrestricted' } },
        betas: BETAS,
      });
      assert.equal(env.type, 'environment');
      assert.ok(env.id.startsWith('env_'), `env id: ${env.id}`);
      pass(`environment created: ${env.id}`);

      // vault + mcp_oauth credential (secret is write-only)
      const vault = await client.beta.vaults.create({ display_name: 'full-e2e vault', betas: BETAS });
      const cred = await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixture.url,
        access_token: CALC_TOKEN,
        betas: BETAS,
      });
      assert.equal(cred.auth.mcp_server_url, fixture.url);
      assert.ok(!JSON.stringify(cred).includes(CALC_TOKEN), 'the access token is never echoed');
      pass(`vault + mcp credential created: ${vault.id}`);

      // file
      const file = await client.beta.files.upload({
        file: await toFile(Buffer.from(`the marker is ${FILE_MARK}`), 'notes.txt'),
        betas: BETAS,
      });
      assert.ok(file.id, 'files.upload returned an id');
      pass(`file uploaded: ${file.id}`);

      // memory store + a seeded memory
      const store = await client.beta.memoryStores.create({
        name: 'kb',
        description: 'seeded knowledge base',
        betas: MEMORY_BETAS,
      });
      assert.equal(store.type, 'memory_store');
      const seeded = await client.beta.memoryStores.memories.create(store.id, {
        path: '/kb.md',
        content: `remember: ${MEM_MARK}`,
        betas: MEMORY_BETAS,
      });
      assert.equal(seeded.path, '/kb.md');
      pass(`memory store created + seeded: ${store.id}`);

      // Agent registry MCP invariant / decision table:
      // server + matching enabled mcp_toolset -> publishable and executable;
      // server without toolset -> 400 (covered by contract-guard E2E). This
      // lifecycle tests execution, so it opts into the tool explicitly.
      const agent = await client.beta.agents.create({
        name: 'assistant',
        // The scenario composition publishes one exact in-process model. Naming
        // that model keeps this Agent executable; an arbitrary provider model
        // would correctly remain an unresolved draft and could not contribute
        // an execution policy to a Session.
        model: 'management',
        system: 'be helpful',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        tools: [calcToolset()],
        metadata: { team: 'full-e2e' },
        betas: BETAS,
      });
      assert.ok(agent.id.startsWith('agent_'), `agent id: ${agent.id}`);
      assert.equal(agent.version, 1);
      assert.deepEqual(agent.tools, [{
        type: 'mcp_toolset',
        mcp_server_name: 'calc',
        configs: [],
        default_config: {
          enabled: true,
          permission_policy: { type: 'always_allow' },
        },
      }], `created Agent retains its typed MCP policy: ${JSON.stringify(agent.tools)}`);
      pass(`agent created in the registry: ${agent.id} (v${agent.version})`);

      // ── 2. CREATE A SESSION THAT ASSOCIATES EVERYTHING ─────────────────────

      const resourceSession = await client.beta.sessions.create({
        agent: {
          id: agent.id,
          type: 'agent_with_overrides',
          mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
          tools: [calcToolset()],
        }, // associate the session with the registry agent and explicit MCP policy
        environment_id: env.id,
        vault_ids: [vault.id],
        resources: [
          { type: 'file', file_id: file.id, mount_path: '/notes.txt' },
          { type: 'memory_store', memory_store_id: store.id, mount_path: '/mnt/memory/kb.md' },
          {
            type: 'github_repository',
            url: 'https://github.com/octocat/Hello-World',
            mount_path: '/workspace/repo',
          },
        ],
        betas: BETAS,
      });
      assert.equal(resourceSession.type, 'session');
      assert.equal(resourceSession.agent.id, agent.id, 'session is associated with the created agent');
      assert.deepEqual(
        resourceSession.agent.tools,
        agent.tools,
        `session freezes the Agent tool policy: ${JSON.stringify(resourceSession.agent.tools)}`,
      );
      assert.equal(resourceSession.environment_id, env.id, 'session is pinned to the environment');
      assert.ok((resourceSession.vault_ids ?? []).includes(vault.id), 'session carries the vault binding');
      pass(`resource session created + associated: ${resourceSession.id}`);

      // The complete create-time snapshot is backfilled; live add remains the
      // official file-only subresource operation.
      const resources = await drain(client.beta.sessions.resources.list(resourceSession.id, { betas: BETAS }));
      const resTypes = resources.map((r) => r.type).sort();
      for (const want of ['file', 'github_repository', 'memory_store']) {
        assert.ok(resTypes.includes(want), `resources.list carries a ${want} (got ${resTypes})`);
      }
      pass(`session resources associated: ${resTypes.join(', ')}`);

      // Local is explicitly selected by the generic deterministic harness and
      // cannot enforce a read-only File mount. Execution must fail closed rather
      // than silently weakening the resource contract.
      const modelRequestsBeforeMount = upstream.requests.length;
      const toolCallsBeforeMount = fixture.calls.filter((call) => call.method === 'tools/call').length;
      const mountReceipt = await send(client, resourceSession.id, 'must fail closed');
      const acceptedMount = mountReceipt.data[0];
      assert.equal(acceptedMount?.type, 'user.message', 'R2 exact User Event receipt family');
      assert.equal(acceptedMount?.processed_at, null, 'R2 effect failure is not falsely processed');
      // The request/CAS is already durable, so a later capability repair retries
      // this same command. Observe one bounded reconciliation window: it may expose
      // nonterminal lifecycle state, but must not invent model/tool/terminal effects
      // or compensate by deleting the retained command.
      await new Promise((resolve) => setTimeout(resolve, 750));
      const pendingMount = await listEvents(client, resourceSession.id);
      const retainedMount = pendingMount.find((event) => event.id === acceptedMount.id);
      assert.equal(retainedMount?.processed_at, null, 'R2 retained history preserves retryability');
      assert.ok(
        !pendingMount.some((event) => [
          'agent.message',
          'agent.mcp_tool_use',
          'agent.mcp_tool_result',
          'agent.tool_use',
          'agent.tool_result',
          'session.error',
          'session.status_idle',
          'session.thread_status_idle',
          'session.usage',
          'span.model_request_start',
          'span.model_request_end',
        ].includes(event.type)),
        `R2 no model/tool/terminal effect is fabricated: ${pendingMount.map((event) => event.type)}`,
      );
      assert.equal(upstream.requests.length, modelRequestsBeforeMount, 'R2 no Provider request');
      assert.equal(
        fixture.calls.filter((call) => call.method === 'tools/call').length,
        toolCallsBeforeMount,
        'R2 no MCP tool execution',
      );
      pass('local read-only capability failure retains one retryable command without effects');

      // The execution half of this broad API lifecycle has no mount requirement;
      // dedicated sandbox provisioning tests exercise actual resource mounts on
      // backends that can enforce them.
      const session = await client.beta.sessions.create({
        agent: {
          id: agent.id,
          type: 'agent_with_overrides',
          mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
          tools: [calcToolset()],
        },
        environment_id: env.id,
        vault_ids: [vault.id],
        betas: BETAS,
      });
      pass(`execution session created without unenforceable mounts: ${session.id}`);

      // ── 3. RUN ─────────────────────────────────────────────────────────────

      // MCP tool Run: the deterministic model calls the calc MCP tool with the
      // vault-materialized bearer, and reports the sum.
      const mcpReceipt = await send(client, session.id, 'add 2 3');
      let events = await committedRunEvents(
        client,
        session.id,
        mcpReceipt,
        (delta) => delta.some((event) => event.type === 'agent.mcp_tool_result'),
        'the MCP Run to commit its result and terminal Session status',
      );
      // An MCP tool (`mcp__server__tool`) projects as the DISTINCT `agent.mcp_tool_use`
      // / `agent.mcp_tool_result` events, not the built-in `agent.tool_use` — the wire
      // distinguishes a host-executed MCP call from a built-in one by the `mcp__` name.
      const toolUse = events.find((e) => e.type === 'agent.mcp_tool_use');
      assert.ok(toolUse, `an agent.mcp_tool_use event (types: ${events.map((e) => e.type)})`);
      assert.equal(toolUse.name, 'mcp__calc__add');
      const toolResult = events.find((e) => e.type === 'agent.mcp_tool_result');
      assert.ok(toolResult, `an agent.mcp_tool_result event: ${JSON.stringify(events)}`);
      assert.equal(toolResult.content[0].text, '5');
      assert.ok(agentMessages(events).some((m) => m.includes('result: 5')), 'final message reports result: 5');
      pass('Run 1: add 2 3 -> mcp__calc__add -> tool_result 5 -> "result: 5"');

      // plain echo Run on the same Session
      const echoReceipt = await send(client, session.id, 'ping');
      events = await committedRunEvents(
        client,
        session.id,
        echoReceipt,
        (delta) => delta.some((event) => event.type === 'agent.message'),
        'the echo Run to commit its reply and terminal Session status',
      );
      assert.ok(agentMessages(events).includes('Echo: ping'), 'plain message echoes');
      pass('Run 2: plain message -> Echo reply');

      // ── 4. CHECK EXISTING + PRODUCED RESOURCES ─────────────────────────────

      // existing: the control-plane objects survive and read back
      const gotEnv = await client.beta.environments.retrieve(env.id, { betas: BETAS });
      assert.equal(gotEnv.id, env.id);
      const gotAgent = await client.beta.agents.retrieve(agent.id, { betas: BETAS });
      assert.ok(
        (gotAgent.mcp_servers ?? []).some((m) => m.name === 'calc'),
        'the agent retains its declared MCP server',
      );
      const memList = await drain(client.beta.memoryStores.memories.list(store.id, { betas: MEMORY_BETAS }));
      assert.ok(
        memList.some((m) => m.type === 'memory' && m.path === '/kb.md'),
        'the seeded memory is listed',
      );
      const gotMem = await client.beta.memoryStores.memories.retrieve(seeded.id, {
        memory_store_id: store.id,
        betas: MEMORY_BETAS,
      });
      assert.ok((gotMem.content ?? '').includes(MEM_MARK), 'the seeded memory content reads back');
      // (a global files.list() is session-output-scoped in this build; the uploaded
      // input file is verified by its metadata + its presence in resources.list.)
      const gotFile = await client.beta.files.retrieveMetadata(file.id, { betas: BETAS });
      assert.equal(gotFile.id, file.id, 'the uploaded file reads back by id');
      pass('existing resources verified: environment, agent (+mcp), memory (+content), file');

      // produced: the run materialized the vault bearer onto the MCP wire, and the
      // fixture recorded exactly the tools/call it served.
      const toolCalls = fixture.calls.filter((c) => c.method === 'tools/call');
      assert.ok(toolCalls.length >= 1, `at least one tools/call reached the MCP server (got ${toolCalls.length})`);
      assert.ok(
        toolCalls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
        'every tools/call carried the vault-materialized bearer',
      );
      const finalSession = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
      assert.equal(finalSession.status, 'idle', 'the session settled idle after the run');
      pass('produced effects verified: MCP tool_use/result events + vault bearer on the wire');
    });

    console.log('E2E PASS: full managed lifecycle (create resources -> associate -> run -> verify) via the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
  }
}

main();
