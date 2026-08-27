import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import Anthropic, { toFile } from '@anthropic-ai/sdk-current';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const oracle = JSON.parse(fs.readFileSync(
  path.resolve(packageRoot, '../../contracts/anthropic-managed/upstream-oracle.generated.json'),
));
const operations = new Map(oracle.current.operations.map((operation) => [operation.id, operation]));
const beta = ['managed-agents-2026-04-01'];
const fixtureID = 'fixture-id';

function requestPath(request) {
  return new URL(request.url).pathname.replaceAll(fixtureID, '{}');
}

function recordingClient() {
  const requests = [];
  const client = new Anthropic({
    apiKey: crypto.randomUUID(),
    baseURL: 'https://managed.invalid',
    fetch: async (input, init) => {
      const url = typeof input === 'string' ? input : input.url;
      if (!url.startsWith('https://managed.invalid')) return globalThis.fetch(input, init);
      requests.push(new Request(input, init));
      return new Response(JSON.stringify({ data: [], has_more: false }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      });
    },
  });
  return { client, requests };
}

const scenarios = [
  {
    resource: 'agents',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.agents.create', (c) => c.beta.agents.create({ model: 'claude-test', name: 'test', betas: beta })],
      ['beta.agents.retrieve', (c) => c.beta.agents.retrieve(fixtureID, { betas: beta })],
      ['beta.agents.update', (c) => c.beta.agents.update(fixtureID, { name: 'updated', betas: beta })],
      ['beta.agents.list', (c) => c.beta.agents.list({ betas: beta })],
      ['beta.agents.archive', (c) => c.beta.agents.archive(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'deployments',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.deployments.create', (c) => c.beta.deployments.create({
        agent: fixtureID,
        environment_id: fixtureID,
        initial_events: [{ type: 'user.message', content: [{ type: 'text', text: 'test' }] }],
        name: 'test',
        betas: beta,
      })],
      ['beta.deployments.retrieve', (c) => c.beta.deployments.retrieve(fixtureID, { betas: beta })],
      ['beta.deployments.update', (c) => c.beta.deployments.update(fixtureID, { name: 'updated', betas: beta })],
      ['beta.deployments.list', (c) => c.beta.deployments.list({ betas: beta })],
      ['beta.deployments.pause', (c) => c.beta.deployments.pause(fixtureID, { betas: beta })],
      ['beta.deployments.unpause', (c) => c.beta.deployments.unpause(fixtureID, { betas: beta })],
      ['beta.deployments.run', (c) => c.beta.deployments.run(fixtureID, { betas: beta })],
      ['beta.deployments.archive', (c) => c.beta.deployments.archive(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'deploymentRuns',
    phases: ['retrieve', 'list'],
    calls: [
      ['beta.deploymentRuns.retrieve', (c) => c.beta.deploymentRuns.retrieve(fixtureID, { betas: beta })],
      ['beta.deploymentRuns.list', (c) => c.beta.deploymentRuns.list({ betas: beta })],
    ],
  },
  {
    resource: 'dreams',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['beta.dreams.create', (c) => c.beta.dreams.create({
        inputs: [{ memory_store_id: fixtureID }], model: 'claude-test', betas: beta,
      })],
      ['beta.dreams.retrieve', (c) => c.beta.dreams.retrieve(fixtureID, { betas: beta })],
      ['beta.dreams.list', (c) => c.beta.dreams.list({ betas: beta })],
      ['beta.dreams.cancel', (c) => c.beta.dreams.cancel(fixtureID, { betas: beta })],
      ['beta.dreams.archive', (c) => c.beta.dreams.archive(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'environments',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.environments.create', (c) => c.beta.environments.create({ name: 'test', betas: beta })],
      ['beta.environments.retrieve', (c) => c.beta.environments.retrieve(fixtureID, { betas: beta })],
      ['beta.environments.update', (c) => c.beta.environments.update(fixtureID, { name: 'updated', betas: beta })],
      ['beta.environments.list', (c) => c.beta.environments.list({ betas: beta })],
      ['beta.environments.archive', (c) => c.beta.environments.archive(fixtureID, { betas: beta })],
      ['beta.environments.delete', (c) => c.beta.environments.delete(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'files',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['beta.files.upload', async (c) => c.beta.files.upload({
        file: await toFile(Buffer.from('fixture'), 'fixture.txt'), betas: beta,
      })],
      ['beta.files.retrieveMetadata', (c) => c.beta.files.retrieveMetadata(fixtureID, { betas: beta })],
      ['beta.files.list', (c) => c.beta.files.list({ betas: beta })],
      ['beta.files.delete', (c) => c.beta.files.delete(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'memoryStores',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.memoryStores.create', (c) => c.beta.memoryStores.create({ name: 'test', betas: beta })],
      ['beta.memoryStores.retrieve', (c) => c.beta.memoryStores.retrieve(fixtureID, { betas: beta })],
      ['beta.memoryStores.update', (c) => c.beta.memoryStores.update(fixtureID, { description: 'updated', betas: beta })],
      ['beta.memoryStores.list', (c) => c.beta.memoryStores.list({ betas: beta })],
      ['beta.memoryStores.archive', (c) => c.beta.memoryStores.archive(fixtureID, { betas: beta })],
      ['beta.memoryStores.delete', (c) => c.beta.memoryStores.delete(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'models',
    phases: ['retrieve', 'list'],
    calls: [
      ['beta.models.retrieve', (c) => c.beta.models.retrieve(fixtureID, { betas: beta })],
      ['beta.models.list', (c) => c.beta.models.list({ betas: beta })],
    ],
  },
  {
    resource: 'sessions',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.sessions.create', (c) => c.beta.sessions.create({ agent: fixtureID, environment_id: fixtureID, betas: beta })],
      ['beta.sessions.retrieve', (c) => c.beta.sessions.retrieve(fixtureID, { betas: beta })],
      ['beta.sessions.update', (c) => c.beta.sessions.update(fixtureID, { title: 'updated', betas: beta })],
      ['beta.sessions.list', (c) => c.beta.sessions.list({ betas: beta })],
      ['beta.sessions.archive', (c) => c.beta.sessions.archive(fixtureID, { betas: beta })],
      ['beta.sessions.delete', (c) => c.beta.sessions.delete(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'skills',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['beta.skills.create', async (c) => c.beta.skills.create({
        files: [await toFile(Buffer.from('# Skill'), 'SKILL.md')], betas: beta,
      })],
      ['beta.skills.retrieve', (c) => c.beta.skills.retrieve(fixtureID, { betas: beta })],
      ['beta.skills.list', (c) => c.beta.skills.list({ betas: beta })],
      ['beta.skills.delete', (c) => c.beta.skills.delete(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'tunnels',
    phases: ['create', 'retrieve', 'list', 'update', 'terminal'],
    calls: [
      ['beta.tunnels.create', (c) => c.beta.tunnels.create({ display_name: 'test', betas: beta })],
      ['beta.tunnels.retrieve', (c) => c.beta.tunnels.retrieve(fixtureID, { betas: beta })],
      ['beta.tunnels.list', (c) => c.beta.tunnels.list({ betas: beta })],
      ['beta.tunnels.rotateToken', (c) => c.beta.tunnels.rotateToken(fixtureID, { reason: 'test', betas: beta })],
      ['beta.tunnels.archive', (c) => c.beta.tunnels.archive(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'userProfiles',
    phases: ['create', 'retrieve', 'update', 'list'],
    calls: [
      ['beta.userProfiles.create', (c) => c.beta.userProfiles.create({ access_type: 'application', betas: beta })],
      ['beta.userProfiles.retrieve', (c) => c.beta.userProfiles.retrieve(fixtureID, { betas: beta })],
      ['beta.userProfiles.update', (c) => c.beta.userProfiles.update(fixtureID, { name: 'updated', betas: beta })],
      ['beta.userProfiles.list', (c) => c.beta.userProfiles.list({ betas: beta })],
      ['beta.userProfiles.createEnrollmentURL', (c) => c.beta.userProfiles.createEnrollmentURL(fixtureID, { betas: beta })],
    ],
  },
  {
    resource: 'vaults',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.vaults.create', (c) => c.beta.vaults.create({ display_name: 'test', betas: beta })],
      ['beta.vaults.retrieve', (c) => c.beta.vaults.retrieve(fixtureID, { betas: beta })],
      ['beta.vaults.update', (c) => c.beta.vaults.update(fixtureID, { display_name: 'updated', betas: beta })],
      ['beta.vaults.list', (c) => c.beta.vaults.list({ betas: beta })],
      ['beta.vaults.archive', (c) => c.beta.vaults.archive(fixtureID, { betas: beta })],
      ['beta.vaults.delete', (c) => c.beta.vaults.delete(fixtureID, { betas: beta })],
    ],
  },
];

test('official SDK constructs every persistent resource lifecycle against canonical routes', async () => {
  // Cause/effect graph: C1 the current official client, C2 every persistent
  // resource aggregate, and C3 its supported lifecycle transitions produce E1
  // the canonical Awaken method/path and E2 the selected public beta. A renamed
  // SDK method, wrong nesting, missing aggregate phase, or route drift fails
  // before deployed qualification. Server state effects remain owned by the
  // Rust behavior tests referenced from operation-coverage.generated.json.
  const coveredResources = new Set();
  for (const scenario of scenarios) {
    const { client, requests } = recordingClient();
    for (const [id, invoke] of scenario.calls) {
      const expected = operations.get(id);
      assert.ok(expected, `${id} is absent from the current official SDK oracle`);
      await invoke(client);
      const request = requests.shift();
      assert.ok(request, `${id} issued no HTTP request`);
      assert.equal(request.method, expected.method, `${id}: method`);
      assert.equal(requestPath(request), expected.path, `${id}: path`);
      assert.match(request.headers.get('anthropic-beta') ?? '', /managed-agents-2026-04-01/u);
      coveredResources.add(scenario.resource);
    }
    assert.equal(requests.length, 0, `${scenario.resource}: unclaimed requests`);
    assert.ok(scenario.phases.includes('retrieve') || scenario.phases.includes('list'));
  }
  assert.deepEqual(
    coveredResources,
    new Set([
      'agents', 'deploymentRuns', 'deployments', 'dreams', 'environments', 'files',
      'memoryStores', 'models', 'sessions', 'skills', 'tunnels', 'userProfiles', 'vaults',
    ]),
  );
});
