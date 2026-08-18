// Official TS SDK legal Session Resource update: GitHub authorization-token
// rotation is write-only and updates the durable execution pin without changing
// the public resource identity. File resources remain non-rotatable.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { pass, withScenarioServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38344);

await withScenarioServer('management', 'mcp', PORT, async (baseURL) => {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const original = 'github-original-token';
  const rotated = 'github-rotated-token';
  const environment = await client.beta.environments.create({
    name: 'resource-rotation',
    config: { type: 'cloud' },
    betas: BETAS,
  });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: environment.id,
    resources: [{
      type: 'github_repository',
      url: 'https://github.com/awaken/managed-compat.git',
      authorization_token: original,
      mount_path: '/workspace/repository',
      checkout: { type: 'branch', name: 'main' },
    }],
    betas: BETAS,
  });
  const repository = session.resources.find((resource) => resource.type === 'github_repository');
  assert.ok(repository?.id);
  assert.ok(!JSON.stringify(session).includes(original), 'create response never echoes the token');

  const updated = await client.beta.sessions.resources.update(repository.id, {
    session_id: session.id,
    authorization_token: rotated,
    betas: BETAS,
  });
  assert.equal(updated.id, repository.id);
  assert.equal(updated.type, 'github_repository');
  assert.equal(updated.url, repository.url);
  assert.equal(updated.mount_path, repository.mount_path);
  assert.deepEqual(updated.checkout, repository.checkout);
  assert.ok(!JSON.stringify(updated).includes(rotated), 'update response never echoes the token');
  const retrieved = await client.beta.sessions.resources.retrieve(repository.id, {
    session_id: session.id,
    betas: BETAS,
  });
  assert.deepEqual(retrieved, updated, 'retrieve observes the rotated resource projection');
  pass('official sessions.resources.update rotates a GitHub token without identity drift or echo');

  const file = await client.beta.files.upload({
    file: await toFile(Buffer.from('immutable'), 'immutable.txt'),
    betas: BETAS,
  });
  const fileResource = await client.beta.sessions.resources.add(session.id, {
    type: 'file',
    file_id: file.id,
    betas: BETAS,
  });
  await assert.rejects(
    () => client.beta.sessions.resources.update(fileResource.id, {
      session_id: session.id,
      authorization_token: 'must-not-enter', // awaken-allow: secret
      betas: BETAS,
    }),
    (error) => error?.status === 400,
  );
  assert.equal(
    (await client.beta.sessions.resources.retrieve(fileResource.id, {
      session_id: session.id,
      betas: BETAS,
    })).id,
    fileResource.id,
    'rejected File update is non-mutating',
  );
  pass('authorization token updates remain restricted to GitHub repository resources');
}, {
  AWAKEN_MGMT_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
});

console.log('E2E PASS: official TS SDK legal Session Resource credential rotation.');
