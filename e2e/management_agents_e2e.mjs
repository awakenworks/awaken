// The public agent registry, driven by the official Anthropic TypeScript SDK
// (`client.beta.agents.*`): create / retrieve / update / list / archive + version
// history. Any wire-shape drift from the official `BetaManagedAgentsAgent` type
// surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_agents_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withServer('management', 38138, async (baseUrl) => {
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
    });

    console.log('E2E PASS: the agent registry round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
