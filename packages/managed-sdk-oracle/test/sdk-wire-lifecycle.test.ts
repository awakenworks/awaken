import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import Anthropic, { toFile } from '@anthropic-ai/sdk-current';

type Client = InstanceType<typeof Anthropic>;
type Operation = {
  id: string;
  method: string;
  path: string;
  betas: string[];
  transport_query?: string;
};
type Invoke = (client: Client) => Promise<unknown> | unknown;
type Scenario = {
  resource: string;
  phases: string[];
  calls: Array<[string, Invoke]>;
};

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const oracle = JSON.parse(fs.readFileSync(
  path.resolve(packageRoot, '../../contracts/anthropic-managed/upstream-oracle.generated.json'),
  'utf8',
)) as { current: { operations: Operation[] } };
const operations = new Map(oracle.current.operations.map((operation) => [operation.id, operation]));
const managedBeta = ['managed-agents-2026-04-01'];
const dreamBeta = ['dreaming-2026-04-21'];
const memoryBeta = ['agent-memory-2026-07-22'];
const tunnelBeta = ['mcp-tunnels-2026-06-22'];
const userProfileBeta = ['user-profiles-2026-08-18'];
const fixtureID = 'fixture-id';

function requestPath(request: Request) {
  return new URL(request.url).pathname.replaceAll(fixtureID, '{}');
}

function recordingClient() {
  const requests: Request[] = [];
  const client = new Anthropic({
    apiKey: crypto.randomUUID(),
    baseURL: 'https://managed.invalid',
    fetch: async (input, init) => {
      const url = typeof input === 'string'
        ? input
        : input instanceof URL
          ? input.href
          : input.url;
      if (!url.startsWith('https://managed.invalid')) return globalThis.fetch(input, init);
      const request = new Request(input, init);
      requests.push(request);
      if (request.headers.get('accept')?.includes('text/event-stream')) {
        return new Response('', { status: 200, headers: { 'content-type': 'text/event-stream' } });
      }
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
    resource: 'files',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['files.upload', async (c) => c.files.upload({
        file: await toFile(Buffer.from('fixture'), 'fixture.txt'), expires_in_seconds: 3600,
      })],
      ['files.retrieveMetadata', (c) => c.files.retrieveMetadata(fixtureID)],
      ['files.download', (c) => c.files.download(fixtureID)],
      ['files.list', (c) => c.files.list({ ids: [fixtureID] })],
      ['files.delete', (c) => c.files.delete(fixtureID)],
    ],
  },
  {
    resource: 'models',
    phases: ['retrieve', 'list'],
    calls: [
      ['models.retrieve', (c) => c.models.retrieve(fixtureID)],
      ['models.list', (c) => c.models.list()],
    ],
  },
  {
    resource: 'skills',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['skills.create', async (c) => c.skills.create({
        files: [await toFile(Buffer.from('# Skill'), 'SKILL.md')], display_name: 'Fixture',
      })],
      ['skills.retrieve', (c) => c.skills.retrieve(fixtureID)],
      ['skills.list', (c) => c.skills.list()],
      ['skills.delete', (c) => c.skills.delete(fixtureID)],
      ['skills.versions.create', async (c) => c.skills.versions.create(fixtureID, {
        files: [await toFile(Buffer.from('# Skill'), 'SKILL.md')],
      })],
      ['skills.versions.retrieve', (c) => c.skills.versions.retrieve(fixtureID, { skill_id: fixtureID })],
      ['skills.versions.list', (c) => c.skills.versions.list(fixtureID)],
      ['skills.versions.delete', (c) => c.skills.versions.delete(fixtureID, { skill_id: fixtureID })],
    ],
  },
  {
    resource: 'agents',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.agents.create', (c) => c.beta.agents.create({ model: 'claude-test', name: 'test', betas: managedBeta })],
      ['beta.agents.retrieve', (c) => c.beta.agents.retrieve(fixtureID, { betas: managedBeta })],
      ['beta.agents.update', (c) => c.beta.agents.update(fixtureID, { name: 'updated', betas: managedBeta })],
      ['beta.agents.list', (c) => c.beta.agents.list({ betas: managedBeta })],
      ['beta.agents.archive', (c) => c.beta.agents.archive(fixtureID, { betas: managedBeta })],
      ['beta.agents.versions.list', (c) => c.beta.agents.versions.list(fixtureID, { betas: managedBeta })],
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
        betas: managedBeta,
      })],
      ['beta.deployments.retrieve', (c) => c.beta.deployments.retrieve(fixtureID, { betas: managedBeta })],
      ['beta.deployments.update', (c) => c.beta.deployments.update(fixtureID, { name: 'updated', betas: managedBeta })],
      ['beta.deployments.list', (c) => c.beta.deployments.list({ betas: managedBeta })],
      ['beta.deployments.pause', (c) => c.beta.deployments.pause(fixtureID, { betas: managedBeta })],
      ['beta.deployments.unpause', (c) => c.beta.deployments.unpause(fixtureID, { betas: managedBeta })],
      ['beta.deployments.run', (c) => c.beta.deployments.run(fixtureID, { betas: managedBeta })],
      ['beta.deployments.archive', (c) => c.beta.deployments.archive(fixtureID, { betas: managedBeta })],
    ],
  },
  {
    resource: 'deploymentRuns',
    phases: ['retrieve', 'list'],
    calls: [
      ['beta.deploymentRuns.retrieve', (c) => c.beta.deploymentRuns.retrieve(fixtureID, { betas: managedBeta })],
      ['beta.deploymentRuns.list', (c) => c.beta.deploymentRuns.list({ betas: managedBeta })],
    ],
  },
  {
    resource: 'dreams',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['beta.dreams.create', (c) => c.beta.dreams.create({
        inputs: [{ type: 'memory_store', memory_store_id: fixtureID }], model: 'claude-test', betas: dreamBeta,
      })],
      ['beta.dreams.retrieve', (c) => c.beta.dreams.retrieve(fixtureID, { betas: dreamBeta })],
      ['beta.dreams.list', (c) => c.beta.dreams.list({ betas: dreamBeta })],
      ['beta.dreams.cancel', (c) => c.beta.dreams.cancel(fixtureID, { betas: dreamBeta })],
      ['beta.dreams.archive', (c) => c.beta.dreams.archive(fixtureID, { betas: dreamBeta })],
    ],
  },
  {
    resource: 'environments',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.environments.create', (c) => c.beta.environments.create({ name: 'test', betas: managedBeta })],
      ['beta.environments.retrieve', (c) => c.beta.environments.retrieve(fixtureID, { betas: managedBeta })],
      ['beta.environments.update', (c) => c.beta.environments.update(fixtureID, { name: 'updated', betas: managedBeta })],
      ['beta.environments.list', (c) => c.beta.environments.list({ betas: managedBeta })],
      ['beta.environments.archive', (c) => c.beta.environments.archive(fixtureID, { betas: managedBeta })],
      ['beta.environments.delete', (c) => c.beta.environments.delete(fixtureID, { betas: managedBeta })],
      ['beta.environments.work.retrieve', (c) => c.beta.environments.work.retrieve(fixtureID, { environment_id: fixtureID, betas: managedBeta })],
      ['beta.environments.work.update', (c) => c.beta.environments.work.update(fixtureID, { environment_id: fixtureID, metadata: { phase: 'test' }, betas: managedBeta })],
      ['beta.environments.work.list', (c) => c.beta.environments.work.list(fixtureID, { betas: managedBeta })],
      ['beta.environments.work.ack', (c) => c.beta.environments.work.ack(fixtureID, { environment_id: fixtureID, betas: managedBeta })],
      ['beta.environments.work.heartbeat', (c) => c.beta.environments.work.heartbeat(fixtureID, { environment_id: fixtureID, betas: managedBeta })],
      ['beta.environments.work.poll', (c) => c.beta.environments.work.poll(fixtureID, { betas: managedBeta })],
      ['beta.environments.work.stats', (c) => c.beta.environments.work.stats(fixtureID, { betas: managedBeta })],
      ['beta.environments.work.stop', (c) => c.beta.environments.work.stop(fixtureID, { environment_id: fixtureID, betas: managedBeta })],
    ],
  },
  {
    resource: 'files',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['beta.files.upload', async (c) => c.beta.files.upload({
        file: await toFile(Buffer.from('fixture'), 'fixture.txt'),
      })],
      ['beta.files.retrieveMetadata', (c) => c.beta.files.retrieveMetadata(fixtureID)],
      ['beta.files.download', (c) => c.beta.files.download(fixtureID)],
      ['beta.files.list', (c) => c.beta.files.list()],
      ['beta.files.delete', (c) => c.beta.files.delete(fixtureID)],
    ],
  },
  {
    resource: 'memoryStores',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.memoryStores.create', (c) => c.beta.memoryStores.create({ name: 'test', betas: memoryBeta })],
      ['beta.memoryStores.retrieve', (c) => c.beta.memoryStores.retrieve(fixtureID, { betas: memoryBeta })],
      ['beta.memoryStores.update', (c) => c.beta.memoryStores.update(fixtureID, { description: 'updated', betas: memoryBeta })],
      ['beta.memoryStores.list', (c) => c.beta.memoryStores.list({ betas: memoryBeta })],
      ['beta.memoryStores.archive', (c) => c.beta.memoryStores.archive(fixtureID, { betas: memoryBeta })],
      ['beta.memoryStores.delete', (c) => c.beta.memoryStores.delete(fixtureID, { betas: memoryBeta })],
      ['beta.memoryStores.memories.create', (c) => c.beta.memoryStores.memories.create(fixtureID, { content: 'fixture', path: '/fixture', betas: memoryBeta })],
      ['beta.memoryStores.memories.retrieve', (c) => c.beta.memoryStores.memories.retrieve(fixtureID, { memory_store_id: fixtureID, betas: memoryBeta })],
      ['beta.memoryStores.memories.update', (c) => c.beta.memoryStores.memories.update(fixtureID, { memory_store_id: fixtureID, content: 'updated', betas: memoryBeta })],
      ['beta.memoryStores.memories.list', (c) => c.beta.memoryStores.memories.list(fixtureID, { betas: memoryBeta })],
      ['beta.memoryStores.memories.delete', (c) => c.beta.memoryStores.memories.delete(fixtureID, { memory_store_id: fixtureID, betas: memoryBeta })],
      ['beta.memoryStores.memoryVersions.retrieve', (c) => c.beta.memoryStores.memoryVersions.retrieve(fixtureID, { memory_store_id: fixtureID, betas: memoryBeta })],
      ['beta.memoryStores.memoryVersions.list', (c) => c.beta.memoryStores.memoryVersions.list(fixtureID, { betas: memoryBeta })],
      ['beta.memoryStores.memoryVersions.redact', (c) => c.beta.memoryStores.memoryVersions.redact(fixtureID, { memory_store_id: fixtureID, betas: memoryBeta })],
    ],
  },
  {
    resource: 'models',
    phases: ['retrieve', 'list'],
    calls: [
      ['beta.models.retrieve', (c) => c.beta.models.retrieve(fixtureID)],
      ['beta.models.list', (c) => c.beta.models.list()],
    ],
  },
  {
    resource: 'sessions',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.sessions.create', (c) => c.beta.sessions.create({ agent: fixtureID, environment_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.retrieve', (c) => c.beta.sessions.retrieve(fixtureID, { betas: managedBeta })],
      ['beta.sessions.update', (c) => c.beta.sessions.update(fixtureID, { title: 'updated', betas: managedBeta })],
      ['beta.sessions.list', (c) => c.beta.sessions.list({ betas: managedBeta })],
      ['beta.sessions.archive', (c) => c.beta.sessions.archive(fixtureID, { betas: managedBeta })],
      ['beta.sessions.delete', (c) => c.beta.sessions.delete(fixtureID, { betas: managedBeta })],
      ['beta.sessions.events.list', (c) => c.beta.sessions.events.list(fixtureID, { betas: managedBeta })],
      ['beta.sessions.events.send', (c) => c.beta.sessions.events.send(fixtureID, { events: [{ type: 'user.message', content: [{ type: 'text', text: 'fixture' }] }], betas: managedBeta })],
      ['beta.sessions.events.stream', (c) => c.beta.sessions.events.stream(fixtureID, { betas: managedBeta })],
      ['beta.sessions.resources.add', (c) => c.beta.sessions.resources.add(fixtureID, { type: 'file', file_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.resources.retrieve', (c) => c.beta.sessions.resources.retrieve(fixtureID, { session_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.resources.update', (c) => c.beta.sessions.resources.update(fixtureID, { session_id: fixtureID, authorization_token: 'fixture', betas: managedBeta })],
      ['beta.sessions.resources.list', (c) => c.beta.sessions.resources.list(fixtureID, { betas: managedBeta })],
      ['beta.sessions.resources.delete', (c) => c.beta.sessions.resources.delete(fixtureID, { session_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.threads.retrieve', (c) => c.beta.sessions.threads.retrieve(fixtureID, { session_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.threads.list', (c) => c.beta.sessions.threads.list(fixtureID, { betas: managedBeta })],
      ['beta.sessions.threads.archive', (c) => c.beta.sessions.threads.archive(fixtureID, { session_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.threads.events.list', (c) => c.beta.sessions.threads.events.list(fixtureID, { session_id: fixtureID, betas: managedBeta })],
      ['beta.sessions.threads.events.stream', (c) => c.beta.sessions.threads.events.stream(fixtureID, { session_id: fixtureID, betas: managedBeta })],
    ],
  },
  {
    resource: 'skills',
    phases: ['create', 'retrieve', 'list', 'terminal'],
    calls: [
      ['beta.skills.create', async (c) => c.beta.skills.create({
        files: [await toFile(Buffer.from('# Skill'), 'SKILL.md')],
      })],
      ['beta.skills.retrieve', (c) => c.beta.skills.retrieve(fixtureID)],
      ['beta.skills.list', (c) => c.beta.skills.list()],
      ['beta.skills.delete', (c) => c.beta.skills.delete(fixtureID)],
      ['beta.skills.versions.create', async (c) => c.beta.skills.versions.create(fixtureID, { files: [await toFile(Buffer.from('# Skill'), 'SKILL.md')] })],
      ['beta.skills.versions.retrieve', (c) => c.beta.skills.versions.retrieve(fixtureID, { skill_id: fixtureID })],
      ['beta.skills.versions.list', (c) => c.beta.skills.versions.list(fixtureID)],
      ['beta.skills.versions.download', (c) => c.beta.skills.versions.download(fixtureID, { skill_id: fixtureID })],
      ['beta.skills.versions.delete', (c) => c.beta.skills.versions.delete(fixtureID, { skill_id: fixtureID })],
    ],
  },
  {
    resource: 'tunnels',
    phases: ['create', 'retrieve', 'list', 'update', 'terminal'],
    calls: [
      ['beta.tunnels.create', (c) => c.beta.tunnels.create({ display_name: 'test', betas: tunnelBeta })],
      ['beta.tunnels.retrieve', (c) => c.beta.tunnels.retrieve(fixtureID, { betas: tunnelBeta })],
      ['beta.tunnels.list', (c) => c.beta.tunnels.list({ betas: tunnelBeta })],
      ['beta.tunnels.revealToken', (c) => c.beta.tunnels.revealToken(fixtureID, { betas: tunnelBeta })],
      ['beta.tunnels.rotateToken', (c) => c.beta.tunnels.rotateToken(fixtureID, { reason: 'test', betas: tunnelBeta })],
      ['beta.tunnels.archive', (c) => c.beta.tunnels.archive(fixtureID, { betas: tunnelBeta })],
      ['beta.tunnels.certificates.create', (c) => c.beta.tunnels.certificates.create(fixtureID, { ca_certificate_pem: 'pem', betas: tunnelBeta })],
      ['beta.tunnels.certificates.retrieve', (c) => c.beta.tunnels.certificates.retrieve(fixtureID, { tunnel_id: fixtureID, betas: tunnelBeta })],
      ['beta.tunnels.certificates.list', (c) => c.beta.tunnels.certificates.list(fixtureID, { betas: tunnelBeta })],
      ['beta.tunnels.certificates.archive', (c) => c.beta.tunnels.certificates.archive(fixtureID, { tunnel_id: fixtureID, betas: tunnelBeta })],
    ],
  },
  {
    resource: 'userProfiles',
    phases: ['create', 'retrieve', 'update', 'list'],
    calls: [
      ['beta.userProfiles.create', (c) => c.beta.userProfiles.create({ access_type: 'application', betas: userProfileBeta })],
      ['beta.userProfiles.retrieve', (c) => c.beta.userProfiles.retrieve(fixtureID, { betas: userProfileBeta })],
      ['beta.userProfiles.update', (c) => c.beta.userProfiles.update(fixtureID, { name: 'updated', betas: userProfileBeta })],
      ['beta.userProfiles.list', (c) => c.beta.userProfiles.list({ betas: userProfileBeta })],
      ['beta.userProfiles.createEnrollmentURL', (c) => c.beta.userProfiles.createEnrollmentURL(fixtureID, { betas: userProfileBeta })],
    ],
  },
  {
    resource: 'vaults',
    phases: ['create', 'retrieve', 'update', 'list', 'terminal'],
    calls: [
      ['beta.vaults.create', (c) => c.beta.vaults.create({ display_name: 'test', betas: managedBeta })],
      ['beta.vaults.retrieve', (c) => c.beta.vaults.retrieve(fixtureID, { betas: managedBeta })],
      ['beta.vaults.update', (c) => c.beta.vaults.update(fixtureID, { display_name: 'updated', betas: managedBeta })],
      ['beta.vaults.list', (c) => c.beta.vaults.list({ betas: managedBeta })],
      ['beta.vaults.archive', (c) => c.beta.vaults.archive(fixtureID, { betas: managedBeta })],
      ['beta.vaults.delete', (c) => c.beta.vaults.delete(fixtureID, { betas: managedBeta })],
      ['beta.vaults.credentials.create', (c) => c.beta.vaults.credentials.create(fixtureID, { auth: { type: 'static_bearer', token: 'fixture', mcp_server_url: 'https://mcp.example' }, display_name: 'fixture', betas: managedBeta })],
      ['beta.vaults.credentials.retrieve', (c) => c.beta.vaults.credentials.retrieve(fixtureID, { vault_id: fixtureID, betas: managedBeta })],
      ['beta.vaults.credentials.update', (c) => c.beta.vaults.credentials.update(fixtureID, { vault_id: fixtureID, display_name: 'updated', betas: managedBeta })],
      ['beta.vaults.credentials.list', (c) => c.beta.vaults.credentials.list(fixtureID, { betas: managedBeta })],
      ['beta.vaults.credentials.mcpOAuthValidate', (c) => c.beta.vaults.credentials.mcpOAuthValidate(fixtureID, { vault_id: fixtureID, betas: managedBeta })],
      ['beta.vaults.credentials.archive', (c) => c.beta.vaults.credentials.archive(fixtureID, { vault_id: fixtureID, betas: managedBeta })],
      ['beta.vaults.credentials.delete', (c) => c.beta.vaults.credentials.delete(fixtureID, { vault_id: fixtureID, betas: managedBeta })],
    ],
  },
] satisfies Scenario[];

test('official SDK constructs every persistent resource lifecycle against canonical routes', async () => {
  // Cause/effect graph: C1 the current official client, C2 every persistent
  // resource aggregate, and C3 its supported lifecycle transitions produce E1
  // the canonical Awaken method/path and E2 the selected public beta. A renamed
  // SDK method, wrong nesting, missing aggregate phase, or route drift fails
  // before deployed qualification. Server state effects remain owned by the
  // Rust behavior tests referenced from operation-coverage.generated.json.
  const coveredResources = new Set();
  const coveredOperations = new Set();
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
      const requestURL = new URL(request.url);
      assert.equal(
        requestURL.searchParams.get('beta'),
        expected.transport_query === 'beta=true' ? 'true' : null,
        `${id}: exact Beta/GA transport selector`,
      );
      const actualBetas = [...new Set((request.headers.get('anthropic-beta') ?? '')
        .split(',')
        .map((value) => value.trim())
        .filter(Boolean))]
        .sort();
      assert.deepEqual(actualBetas, [...expected.betas].sort(), `${id}: exact beta capability`);
      assert.ok(!coveredOperations.has(id), `${id}: duplicate transport case`);
      coveredOperations.add(id);
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
  assert.deepEqual(
    coveredOperations,
    new Set(oracle.current.operations.map(({ id }) => id)),
    'every current official SDK operation has one executable transport case',
  );
});
