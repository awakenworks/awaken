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
    await withScenarioServer('management', 'mcp', 38138, async (baseUrl) => {
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

      // A stale version conflicts.
      await assert.rejects(
        () => client.beta.agents.update(agent.id, { version: 1, name: 'nope', betas: BETAS }),
        (err) => err.status === 409,
      );
      pass('beta.agents.update(stale version) -> 409');

      const versions = await drain(client.beta.agents.versions.list(agent.id, { betas: BETAS }));
      assert.deepEqual(versions.map((v) => v.version), [1, 2]);
      pass(`beta.agents.versions.list -> ${versions.length} versions`);

      const ids = (await drain(client.beta.agents.list({ betas: BETAS }))).map((a) => a.id);
      assert.ok(ids.includes(agent.id));
      pass('beta.agents.list -> PageCursor<BetaManagedAgentsAgent>');

      const archived = await client.beta.agents.archive(agent.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archived agent carries archived_at');
      pass('beta.agents.archive -> archived_at set');

      // Exercise the complete authoring projection rather than only name/model.
      // String and object tool spellings are normalized; malformed entries are
      // ignored. JSON null deserializes as an absent optional patch and therefore
      // leaves the existing multiagent binding unchanged.
      const rich = await json(baseUrl, 'POST', '/v1/agents', {
        name: 'rich-agent',
        model: { id: 'claude-sonnet-5', speed: 'fast' },
        description: 'all mutable fields',
        system: 'rich system',
        metadata: { team: 'platform' },
        mcp_servers: [{ name: 'docs', type: 'url', url: 'https://example.invalid/mcp' }],
        skills: [{ id: 'skill-a' }],
        tools: ['bash', { id: 'glob' }, { name: 'read' }, 7, null],
        multiagent: { enabled: true },
      });
      assert.equal(rich.status, 200, JSON.stringify(rich.body));
      assert.deepEqual(rich.body.tools.map((tool) => tool.name), ['bash', 'glob', 'read']);
      assert.deepEqual(rich.body.multiagent, { enabled: true });

      const richUpdated = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}`, {
        version: rich.body.version,
        description: 'replaced',
        system: 'replaced system',
        metadata: { team: 'runtime' },
        mcp_servers: [],
        skills: [],
        tools: [{ name: 'write' }],
        multiagent: null,
      });
      assert.equal(richUpdated.status, 200, JSON.stringify(richUpdated.body));
      assert.equal(richUpdated.body.description, 'replaced');
      assert.equal(richUpdated.body.system, 'replaced system');
      assert.deepEqual(richUpdated.body.metadata, { team: 'runtime' });
      assert.deepEqual(richUpdated.body.mcp_servers, []);
      assert.deepEqual(richUpdated.body.skills, []);
      assert.deepEqual(richUpdated.body.tools.map((tool) => tool.name), ['write']);
      assert.deepEqual(richUpdated.body.multiagent, { enabled: true });

      const richArchived = await json(baseUrl, 'POST', `/v1/agents/${rich.body.id}/archive`);
      assert.equal(richArchived.status, 200);
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
        ['POST', '/v1/agents/agent_missing/archive', undefined],
        ['GET', '/v1/agents/agent_missing/versions', undefined],
      ]) {
        assert.equal((await json(baseUrl, method, route, body)).status, 404, route);
      }

      const firstPage = await json(baseUrl, 'GET', '/v1/agents?limit=1');
      assert.equal(firstPage.status, 200);
      assert.equal(firstPage.body.data.length, 1);
      assert.equal(firstPage.body.has_more, true);
      const secondPage = await json(
        baseUrl,
        'GET',
        `/v1/agents?limit=10&page=${encodeURIComponent(firstPage.body.next_page)}`,
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
