import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';

import { loadQualifiedClients, qualifiedClient } from './clients.mjs';

const managedBetas = ['managed-agents-2026-04-01'];
const fileBetas = [...managedBetas, 'files-api-2025-04-14'];

async function drain(page) {
  const values = [];
  for await (const value of page) values.push(value);
  return values;
}

export async function prepareRecovery({ client, toFile, agent, environmentId, marker }) {
  const commandKey = `managed-recovery-${marker}`;
  const file = await client.beta.files.upload({
    file: await toFile(Buffer.from(`managed recovery ${marker}`), `${marker}.txt`),
    betas: fileBetas,
  });
  const params = {
    agent,
    environment_id: environmentId,
    title: `Managed recovery ${marker}`,
    metadata: { qualification_marker: marker },
    resources: [{ type: 'file', file_id: file.id, mount_path: `/workspace/${marker}.txt` }],
    betas: fileBetas,
  };
  const options = { headers: { 'idempotency-key': commandKey } };
  const attempts = await Promise.allSettled([
    client.beta.sessions.create(params, options),
    client.beta.sessions.create(params, options),
  ]);
  const failed = attempts.filter(({ status }) => status === 'rejected');
  if (failed.length > 0) {
    const created = attempts
      .filter(({ status }) => status === 'fulfilled')
      .map(({ value }) => value.id);
    await Promise.allSettled([
      ...[...new Set(created)].map(
        (sessionID) => client.beta.sessions.delete(sessionID, { betas: managedBetas }),
      ),
      client.beta.files.delete(file.id, { betas: fileBetas }),
    ]);
    throw new AggregateError(failed.map(({ reason }) => reason), 'concurrent recovery prepare failed');
  }
  const [first, concurrentReplay] = attempts.map(({ value }) => value);
  assert.equal(concurrentReplay.id, first.id, 'concurrent command converges to one Session');
  return {
    schema_version: 1,
    marker,
    command_key: commandKey,
    session_id: first.id,
    file_id: file.id,
    agent,
    environment_id: environmentId,
  };
}

export async function verifyRecovery({ client, state }) {
  assert.equal(state.schema_version, 1, 'recovery state schema');
  const session = await client.beta.sessions.retrieve(state.session_id, { betas: managedBetas });
  assert.equal(session.metadata?.qualification_marker, state.marker, 'Session metadata survived restart');
  const file = await client.beta.files.retrieveMetadata(state.file_id, { betas: fileBetas });
  assert.equal(file.id, state.file_id, 'File metadata survived restart');
  const resources = await drain(client.beta.sessions.resources.list(state.session_id, { betas: fileBetas }));
  assert.ok(
    resources.some((resource) => resource.type === 'file' && resource.file_id === state.file_id),
    'Session/File relationship survived restart',
  );
  const listed = await drain(client.beta.sessions.list({
    agent_id: state.agent,
    include_archived: true,
    limit: 100,
    order: 'desc',
    betas: managedBetas,
  }));
  assert.ok(listed.some(({ id }) => id === state.session_id), 'pagination reaches recovered Session');

  const params = {
    agent: state.agent,
    environment_id: state.environment_id,
    title: `Managed recovery ${state.marker}`,
    metadata: { qualification_marker: state.marker },
    resources: [{
      type: 'file', file_id: state.file_id, mount_path: `/workspace/${state.marker}.txt`,
    }],
    betas: fileBetas,
  };
  const replay = await client.beta.sessions.create(params, {
    headers: { 'idempotency-key': state.command_key },
  });
  assert.equal(replay.id, state.session_id, 'idempotency receipt survived restart');
  await assert.rejects(
    () => client.beta.sessions.create({ ...params, title: `${params.title} changed` }, {
      headers: { 'idempotency-key': state.command_key }, maxRetries: 0,
    }),
    (error) => error?.status === 409,
    'same command key with changed payload remains a conflict after restart',
  );
}

export async function cleanupRecovery({ client, state }) {
  const cleanup = await Promise.allSettled([
    client.beta.sessions.delete(state.session_id, { betas: managedBetas }),
    client.beta.files.delete(state.file_id, { betas: fileBetas }),
  ]);
  const failed = cleanup.filter(({ status }) => status === 'rejected');
  if (failed.length > 0) {
    throw new AggregateError(failed.map(({ reason }) => reason), 'managed recovery cleanup failed');
  }
}

async function main() {
  const phase = process.argv[2];
  assert.ok(['prepare', 'verify', 'cleanup'].includes(phase), 'phase is prepare, verify, or cleanup');
  const baseURL = process.env.AWAKEN_MANAGED_BASE_URL;
  const apiKey = process.env.AWAKEN_MANAGED_API_KEY;
  const agent = process.env.AWAKEN_MANAGED_AGENT_ID;
  const environmentId = process.env.AWAKEN_MANAGED_ENVIRONMENT_ID;
  const stateFile = process.env.AWAKEN_MANAGED_RECOVERY_STATE_FILE;
  for (const [name, value] of Object.entries({ baseURL, apiKey, agent, environmentId, stateFile })) {
    assert.ok(value, `${name} is required`);
  }
  const current = qualifiedClient(await loadQualifiedClients(), 'current_oracle');
  const client = new current.Client({ apiKey, baseURL });
  if (phase === 'prepare') {
    const marker = `${Date.now()}-${crypto.randomUUID()}`;
    const state = await prepareRecovery({ client, toFile: current.toFile, agent, environmentId, marker });
    fs.writeFileSync(stateFile, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
    return;
  }
  const state = JSON.parse(fs.readFileSync(stateFile, 'utf8'));
  if (phase === 'verify') await verifyRecovery({ client, state });
  else await cleanupRecovery({ client, state });
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) await main();
