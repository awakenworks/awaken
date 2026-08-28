// Representative CRUD and cross-version ownership transfer for every shared
// mutable non-Session Managed resource family. Focused E2Es own the full state
// machines; this suite owns old-create/current-operate and
// current-create/old-operate. The read-only Models catalog remains owned by
// `management_files_models_e2e.mjs`, which provisions its canonical inventory
// and verifies both Beta and GA projections without duplicating that setup here.
//
// Cause/effect graph: creator SDK -> canonical resource row -> operator SDK ->
// update/list/subresource/terminal mutation. Effect: one wire schema and one
// aggregate survive the client change without a compatibility copy.
// Decision table: creator/operator = 0.105/0.117, 0.117/0.105,
// 0.117/current and current/0.117; each resource must cross the handoff and
// terminate through the opposite generated client.

import assert from 'node:assert/strict';
import AnthropicCurrent, { toFile as toFileCurrent } from '@anthropic-ai/sdk';
import Anthropic0105, { toFile as toFile0105 } from '@anthropic-ai/sdk-0-105';
import Anthropic0117, { toFile as toFile0117 } from '@anthropic-ai/sdk-0-117';
import { pass, withScenarioServer } from '../harness.mjs';
import { sdkVersionBinding } from './catalog.mjs';

const CURRENT_SDK_VERSION = sdkVersionBinding().oracle;
const CLIENTS = [
  ['0.105.0', Anthropic0105, toFile0105],
  ['0.117.1', Anthropic0117, toFile0117],
  [CURRENT_SDK_VERSION, AnthropicCurrent, toFileCurrent],
];

async function drain(items) {
  const values = [];
  for await (const item of items) values.push(item);
  return values;
}

function skillMarkdown(name, revision) {
  return `---\nname: ${name}\ndescription: SDK handoff ${revision}\n---\n# ${name}\nRevision ${revision}.\n`;
}

async function exerciseResources(baseURL, creatorSpec, operatorSpec) {
  const [creatorVersion, Creator, creatorToFile] = creatorSpec;
  const [operatorVersion, Operator, operatorToFile] = operatorSpec;
  const creator = new Creator({ apiKey: 'e2e-dummy', baseURL });
  const operator = new Operator({ apiKey: 'e2e-dummy', baseURL });
  const suffix = `${creatorVersion.replaceAll('.', '')}-${operatorVersion.replaceAll('.', '')}`;

  const environment = await creator.beta.environments.create({
    name: `compat-env-${suffix}`,
    config: { type: 'self_hosted' },
  });
  assert.equal((await operator.beta.environments.retrieve(environment.id)).id, environment.id);
  const environmentUpdate = await operator.beta.environments.update(environment.id, {
    description: `updated-by-${operatorVersion}`,
  });
  assert.equal(environmentUpdate.description, `updated-by-${operatorVersion}`);

  const agent = await creator.beta.agents.create({
    name: `compat-agent-${suffix}`,
    model: 'claude-opus-4-8',
    system: 'cross-version fixture',
  });
  assert.equal((await operator.beta.agents.retrieve(agent.id)).id, agent.id);
  const agentUpdate = await operator.beta.agents.update(agent.id, {
    version: agent.version,
    system: `updated-by-${operatorVersion}`,
  });
  assert.equal(agentUpdate.version, agent.version + 1);
  assert.deepEqual(
    (await drain(creator.beta.agents.versions.list(agent.id))).map((item) => item.version),
    [1, 2],
  );

  const deployment = await creator.beta.deployments.create({
    agent: agent.id,
    environment_id: environment.id,
    name: `compat-deployment-${suffix}`,
    initial_events: [{
      type: 'user.message',
      content: [{ type: 'text', text: 'compatibility handoff' }],
    }],
  });
  assert.equal((await operator.beta.deployments.retrieve(deployment.id)).id, deployment.id);
  assert.equal((await operator.beta.deployments.update(deployment.id, {
    description: `updated-by-${operatorVersion}`,
  })).description, `updated-by-${operatorVersion}`);
  assert.equal((await creator.beta.deployments.pause(deployment.id)).status, 'paused');
  assert.equal((await operator.beta.deployments.unpause(deployment.id)).status, 'active');
  assert.ok((await operator.beta.deployments.archive(deployment.id)).archived_at);

  const vault = await creator.beta.vaults.create({
    display_name: `compat-vault-${suffix}`,
    metadata: { creator: creatorVersion },
  });
  assert.equal((await operator.beta.vaults.retrieve(vault.id)).id, vault.id);
  assert.equal((await operator.beta.vaults.update(vault.id, {
    display_name: `compat-vault-updated-${suffix}`,
  })).display_name, `compat-vault-updated-${suffix}`);
  const credential = await creator.beta.vaults.credentials.create(vault.id, {
    type: 'environment_variable',
    secret_name: `SDK_COMPAT_${suffix.replace('-', '_')}`,
    secret_value: 'sdk-compat-placeholder', // awaken-allow: secret
    networking: { type: 'unrestricted' },
  });
  assert.equal((await operator.beta.vaults.credentials.retrieve(credential.id, {
    vault_id: vault.id,
  })).id, credential.id);
  assert.equal((await operator.beta.vaults.credentials.delete(credential.id, {
    vault_id: vault.id,
  })).type, 'vault_credential_deleted');
  assert.ok((await creator.beta.vaults.archive(vault.id)).archived_at);
  assert.equal((await operator.beta.vaults.delete(vault.id)).type, 'vault_deleted');

  const profile = await creator.beta.userProfiles.create({
    external_id: `compat-${suffix}`,
    name: `Profile ${suffix}`,
    relationship: 'external',
  });
  assert.equal((await operator.beta.userProfiles.retrieve(profile.id)).id, profile.id);
  assert.equal((await operator.beta.userProfiles.update(profile.id, {
    name: `Updated ${suffix}`,
  })).name, `Updated ${suffix}`);
  assert.ok((await drain(creator.beta.userProfiles.list())).some((item) => item.id === profile.id));
  assert.equal((await operator.beta.userProfiles.createEnrollmentURL(profile.id)).type, 'enrollment_url');

  const skillName = `compat-${suffix}`;
  const skill = await creator.beta.skills.create({
    display_title: `Compat ${suffix}`,
    files: [await creatorToFile(Buffer.from(skillMarkdown(skillName, 'one')), 'SKILL.md')],
  });
  assert.equal((await operator.beta.skills.retrieve(skill.id)).id, skill.id);
  const secondVersion = await operator.beta.skills.versions.create(skill.id, {
    files: [await operatorToFile(Buffer.from(skillMarkdown(skillName, 'two')), 'SKILL.md')],
  });
  assert.equal(secondVersion.version, '2');
  assert.deepEqual(
    (await drain(creator.beta.skills.versions.list(skill.id))).map((item) => item.version),
    ['1', '2'],
  );
  assert.equal((await operator.beta.skills.delete(skill.id)).type, 'skill_deleted');

  const file = await creator.beta.files.upload({
    file: await creatorToFile(Buffer.from(`file-${suffix}`), `compat-${suffix}.txt`),
  });
  assert.equal((await operator.beta.files.retrieveMetadata(file.id)).id, file.id);
  assert.equal((await operator.beta.files.delete(file.id)).type, 'file_deleted');

  assert.ok((await creator.beta.agents.archive(agent.id)).archived_at);
  assert.ok((await operator.beta.environments.archive(environment.id)).archived_at);
  assert.equal((await creator.beta.environments.delete(environment.id)).type, 'environment_deleted');
  pass(`shared resource CRUD handoff ${creatorVersion} -> ${operatorVersion}`);
}

await withScenarioServer(
  'management',
  'mcp',
  38188,
  async (baseURL) => {
    await exerciseResources(baseURL, CLIENTS[0], CLIENTS[1]);
    await exerciseResources(baseURL, CLIENTS[1], CLIENTS[0]);
    await exerciseResources(baseURL, CLIENTS[1], CLIENTS[2]);
    await exerciseResources(baseURL, CLIENTS[2], CLIENTS[1]);
  },
);

console.log('E2E PASS: all shared mutable Managed resource families survive bidirectional SDK handoff.');
