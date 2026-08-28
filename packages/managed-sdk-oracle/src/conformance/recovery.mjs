import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';

import {
  currentAndCandidateClients,
  loadConformanceClients,
} from './clients.mjs';

const managedBetas = ['managed-agents-2026-04-01'];
const fileBetas = [...managedBetas, 'files-api-2025-04-14'];

export function recoverySdkIdentity(client) {
  return { sdk_version: client.version, sdk_role: client.role };
}

export function assertRecoverySdkIdentity(state, client) {
  const expected = recoverySdkIdentity(client);
  assert.equal(state.sdk_version, expected.sdk_version, 'recovery phases use one exact SDK version');
  assert.equal(state.sdk_role, expected.sdk_role, 'recovery phases use one exact SDK role');
}

const RECOVERY_STATE_FIELDS = Object.freeze([
  'agent',
  'command_key',
  'environment_id',
  'file_id',
  'marker',
  'schema_version',
  'sdk_role',
  'sdk_version',
  'session_id',
]);

export function validateRecoveryState(state) {
  assert.ok(state && typeof state === 'object' && !Array.isArray(state), 'recovery state object');
  assert.deepEqual(Object.keys(state).sort(), [...RECOVERY_STATE_FIELDS].sort(), 'recovery state fields');
  assert.equal(state.schema_version, 1, 'recovery state schema');
  for (const field of RECOVERY_STATE_FIELDS.filter((field) => field !== 'schema_version')) {
    assert.ok(
      typeof state[field] === 'string' && state[field].trim().length > 0,
      `recovery state ${field}`,
    );
  }
  assert.match(state.sdk_version, /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u, 'recovery SDK version');
  assert.ok(
    ['current_oracle', 'candidate'].includes(state.sdk_role),
    'recovery SDK role',
  );
  return state;
}

export function writeRecoveryState(stateFile, state) {
  validateRecoveryState(state);
  assert.equal(fs.existsSync(stateFile), false, 'recovery state file must be new');
  const temporary = `${stateFile}.tmp-${process.pid}`;
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(state, null, 2)}\n`, {
      mode: 0o600,
      flag: 'wx',
    });
    // Publishing through a hard link is both atomic and no-replace. Unlike a
    // preflight exists check followed by rename, a concurrent writer cannot be
    // silently overwritten between those two operations.
    fs.linkSync(temporary, stateFile);
  } finally {
    fs.rmSync(temporary, { force: true });
  }
}

export async function persistPreparedRecovery({ client, stateFile, state, write = writeRecoveryState }) {
  try {
    write(stateFile, state);
  } catch (writeFailure) {
    try {
      await cleanupRecovery({ client, state });
    } catch (cleanupFailure) {
      throw new AggregateError(
        [writeFailure, cleanupFailure],
        'recovery state persistence and compensation both failed',
      );
    }
    throw writeFailure;
  }
}

async function drain(page) {
  const values = [];
  for await (const value of page) values.push(value);
  return values;
}

export async function prepareRecovery({ client, toFile, agent, environmentId, marker }) {
  const commandKey = `managed-recovery-${marker}`;
  const file = await client.beta.files.upload({
    file: await toFile(Buffer.from(`managed recovery ${marker}`), `${marker}.txt`),
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
    const compensation = await Promise.allSettled([
      ...[...new Set(created)].map(
        (sessionID) => client.beta.sessions.delete(sessionID, { betas: managedBetas }),
      ),
      client.beta.files.delete(file.id),
    ]);
    const prepareFailure = new AggregateError(
      failed.map(({ reason }) => reason),
      'concurrent recovery prepare failed',
    );
    const compensationFailed = compensation.filter(({ status }) => status === 'rejected');
    if (compensationFailed.length > 0) {
      throw new AggregateError(
        [
          prepareFailure,
          new AggregateError(
            compensationFailed.map(({ reason }) => reason),
            'concurrent recovery prepare compensation failed',
          ),
        ],
        'concurrent recovery prepare and compensation both failed',
      );
    }
    throw prepareFailure;
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
  const file = await client.beta.files.retrieveMetadata(state.file_id);
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
    client.beta.files.delete(state.file_id),
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
  const selected = currentAndCandidateClients(await loadConformanceClients()).at(-1);
  const client = new selected.Client({ apiKey, baseURL });
  if (phase === 'prepare') {
    const marker = `${Date.now()}-${crypto.randomUUID()}`;
    const state = await prepareRecovery({
      client,
      toFile: selected.toFile,
      agent,
      environmentId,
      marker,
    });
    await persistPreparedRecovery({
      client,
      stateFile,
      state: {
        ...state,
        ...recoverySdkIdentity(selected),
      },
    });
    return;
  }
  const state = validateRecoveryState(JSON.parse(fs.readFileSync(stateFile, 'utf8')));
  assertRecoverySdkIdentity(state, selected);
  if (phase === 'verify') await verifyRecovery({ client, state });
  else await cleanupRecovery({ client, state });
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) await main();
