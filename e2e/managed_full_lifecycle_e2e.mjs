// Full Managed-Agents lifecycle e2e, driven end-to-end by the official Anthropic
// TypeScript SDK against awaken-server-local in `management` mode — the one build
// that wires the whole control plane (environments, the /v1/agents registry,
// vaults + MCP, memory stores, files, sessions + resources) over a deterministic
// executor (`McpToolModel`: "add a b" -> real ext-mcp calc.add, else echo).
//
// The arc the user asked for, in one server:
//   1. CREATE resources: environment, vault + MCP credential, file, memory store
//      (+ seed), and an agent in the registry (declaring the MCP server).
//   2. CREATE a session that ASSOCIATES them: agent id + environment_id + vault_ids
//      + mcp_servers + create-time resources (file, memory_store), then attach a
//      github_repository to the live session.
//   3. RUN it: an MCP-tool turn ("add 2 3" -> mcp__calc__add -> "result: 5") and a
//      plain echo turn.
//   4. CHECK existing resources (env / agent / memory / file / attached resources)
//      and PRODUCED effects (tool_use + tool_result events, and the vault-materialized
//      bearer the MCP fixture saw on the wire).
//
// Run: (from e2e/)  npm install && node managed_full_lifecycle_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const CALC_TOKEN = 'calc-bearer-token-full-e2e'; // awaken-allow: secret
const FILE_MARK = 'FILE_MARK_5150'; // awaken-allow: secret
const MEM_MARK = 'MEM_MARK_2718'; // awaken-allow: secret
const PORT = Number(process.env.E2E_PORT ?? 38195);

async function drain(pageIter) {
  const out = [];
  for await (const item of pageIter) out.push(item);
  return out;
}

async function listEvents(client, sid) {
  return drain(client.beta.sessions.events.list(sid, { betas: BETAS }));
}

async function send(client, sid, text) {
  await client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

const agentMessages = (events) =>
  events.filter((e) => e.type === 'agent.message').flatMap((e) => (e.content ?? []).map((c) => c.text ?? ''));

async function main() {
  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withScenarioServer('management', 'mcp', PORT, async (baseUrl) => {
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
        betas: BETAS,
      });
      assert.equal(store.type, 'memory_store');
      const seeded = await client.beta.memoryStores.memories.create(store.id, {
        path: '/kb.md',
        content: `remember: ${MEM_MARK}`,
        betas: BETAS,
      });
      assert.equal(seeded.path, '/kb.md');
      pass(`memory store created + seeded: ${store.id}`);

      // agent (registry), declaring the MCP server it may use
      const agent = await client.beta.agents.create({
        name: 'assistant',
        model: 'claude-opus-4-8',
        system: 'be helpful',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        metadata: { team: 'full-e2e' },
        betas: BETAS,
      });
      assert.ok(agent.id.startsWith('agent_'), `agent id: ${agent.id}`);
      assert.equal(agent.version, 1);
      pass(`agent created in the registry: ${agent.id} (v${agent.version})`);

      // ── 2. CREATE A SESSION THAT ASSOCIATES EVERYTHING ─────────────────────

      const session = await client.beta.sessions.create({
        agent: agent.id, // associate the session with the registry agent
        environment_id: env.id,
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        vault_ids: [vault.id],
        resources: [
          { type: 'file', file_id: file.id, mount_path: '/notes.txt' },
          { type: 'memory_store', memory_store_id: store.id, mount_path: '/mnt/memory/kb.md' },
        ],
        betas: BETAS,
      });
      assert.equal(session.type, 'session');
      assert.equal(session.agent.id, agent.id, 'session is associated with the created agent');
      assert.equal(session.environment_id, env.id, 'session is pinned to the environment');
      assert.ok((session.vault_ids ?? []).includes(vault.id), 'session carries the vault binding');
      pass(`session created + associated: ${session.id}`);

      // attach a github repository to the LIVE session (association; the actual clone
      // happens only in the git-repo host — here we prove attach + list).
      const repoRes = await client.beta.sessions.resources.add(session.id, {
        type: 'github_repository',
        url: 'https://github.com/octocat/Hello-World',
        mount_path: '/workspace/repo',
        betas: BETAS,
      });
      assert.equal(repoRes.type, 'github_repository');
      pass('github_repository attached to the live session');

      // the create-time + live resources are all backfilled and addressable
      const resources = await drain(client.beta.sessions.resources.list(session.id, { betas: BETAS }));
      const resTypes = resources.map((r) => r.type).sort();
      for (const want of ['file', 'github_repository', 'memory_store']) {
        assert.ok(resTypes.includes(want), `resources.list carries a ${want} (got ${resTypes})`);
      }
      pass(`session resources associated: ${resTypes.join(', ')}`);

      // ── 3. RUN ─────────────────────────────────────────────────────────────

      // MCP tool turn: the deterministic model calls the calc MCP tool with the
      // vault-materialized bearer, and reports the sum.
      await send(client, session.id, 'add 2 3');
      let events = await listEvents(client, session.id);
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.ok(toolUse, `an agent.tool_use event (types: ${events.map((e) => e.type)})`);
      assert.equal(toolUse.name, 'mcp__calc__add');
      const toolResult = events.find((e) => e.type === 'agent.tool_result');
      assert.equal(toolResult.content[0].text, '5');
      assert.ok(agentMessages(events).some((m) => m.includes('result: 5')), 'final message reports result: 5');
      pass('run turn 1: add 2 3 -> mcp__calc__add -> tool_result 5 -> "result: 5"');

      // plain echo turn on the same session
      await send(client, session.id, 'ping');
      events = await listEvents(client, session.id);
      assert.ok(agentMessages(events).includes('Echo: ping'), 'plain message echoes');
      pass('run turn 2: plain message -> Echo reply');

      // ── 4. CHECK EXISTING + PRODUCED RESOURCES ─────────────────────────────

      // existing: the control-plane objects survive and read back
      const gotEnv = await client.beta.environments.retrieve(env.id, { betas: BETAS });
      assert.equal(gotEnv.id, env.id);
      const gotAgent = await client.beta.agents.retrieve(agent.id, { betas: BETAS });
      assert.ok(
        (gotAgent.mcp_servers ?? []).some((m) => m.name === 'calc'),
        'the agent retains its declared MCP server',
      );
      const memList = await drain(client.beta.memoryStores.memories.list(store.id, { betas: BETAS }));
      assert.ok(
        memList.some((m) => m.type === 'memory' && m.path === '/kb.md'),
        'the seeded memory is listed',
      );
      const gotMem = await client.beta.memoryStores.memories.retrieve(seeded.id, {
        memory_store_id: store.id,
        betas: BETAS,
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
