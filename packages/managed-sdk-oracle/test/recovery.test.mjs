import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';

import {
  assertRecoverySdkIdentity,
  cleanupRecovery,
  persistPreparedRecovery,
  prepareRecovery,
  recoverySdkIdentity,
  validateRecoveryState,
  verifyRecovery,
  writeRecoveryState,
} from '../src/conformance/recovery.mjs';

function page(values) {
  return { async *[Symbol.asyncIterator]() { yield* values; } };
}

function recoveryFake() {
  const calls = [];
  const state = {
    session: null,
    file: { id: 'file-1' },
    tunnel: { id: 'tunnel-1', archived_at: null },
    tunnelToken: 'rotated-tunnel-token', // awaken-allow: secret (fixture)
  };
  const client = { beta: {
    files: {
      upload: async () => state.file,
      retrieveMetadata: async () => state.file,
      delete: async (id) => calls.push(['file.delete', id]),
    },
    sessions: {
      create: async (params, options) => {
        calls.push(['session.create', params.title, options.headers['idempotency-key']]);
        if (state.session && params.title !== state.session.title) {
          throw Object.assign(new Error('conflict'), { status: 409 });
        }
        state.session ??= {
          id: 'session-1', title: params.title, metadata: params.metadata,
        };
        return state.session;
      },
      retrieve: async () => state.session,
      list: () => page([state.session]),
      delete: async (id) => calls.push(['session.delete', id]),
      resources: {
        list: () => page([{ type: 'file', file_id: state.file.id }]),
      },
    },
    tunnels: {
      create: async () => {
        calls.push(['tunnel.create', state.tunnel.id]);
        return state.tunnel;
      },
      rotateToken: async (id) => {
        calls.push(['tunnel.rotate', id]);
        return { tunnel_token: state.tunnelToken };
      },
      retrieve: async () => state.tunnel,
      list: () => page([state.tunnel]),
      revealToken: async () => ({ tunnel_token: state.tunnelToken }),
      archive: async (id) => {
        calls.push(['tunnel.archive', id]);
        state.tunnel = { ...state.tunnel, archived_at: '2026-08-29T00:00:00Z' };
        return state.tunnel;
      },
    },
  } };
  return { calls, client };
}

test('recovery protocol proves concurrent convergence and post-restart replay', async () => {
  // Cause/effect graph: C1 two concurrent creates share identity+payload -> E1
  // one Session id; C2 a new client instance observes persisted aggregate,
  // File relation, list cursor and receipt -> E2 exact replay; C3 same identity
  // with changed payload -> E3 409 without mutation; C4 cleanup -> E4 both
  // aggregate and File are explicitly removed.
  const { calls, client } = recoveryFake();
  const state = await prepareRecovery({
    client,
    tunnelClient: client,
    toFile: async (value, name) => ({ value, name }),
    agent: 'agent-1',
    environmentId: 'environment-1',
    marker: 'marker-1',
  });
  assert.equal(calls.filter(([name]) => name === 'session.create').length, 2, 'C1/E1');
  await verifyRecovery({ client, tunnelClient: client, state });
  await cleanupRecovery({ client, tunnelClient: client, state });
  assert.deepEqual(calls.slice(-3), [
    ['session.delete', 'session-1'],
    ['file.delete', 'file-1'],
    ['tunnel.archive', 'tunnel-1'],
  ], 'C4/E4');
});

test('failed concurrent prepare compensates every partially created resource', async () => {
  // FMECA partition: File succeeds, one concurrent Session succeeds, the other
  // fails. Effect: the successful Session and File are both deleted before the
  // failure escapes, so qualification itself cannot pollute staging.
  const deleted = [];
  let creates = 0;
  const client = { beta: {
    files: {
      upload: async () => ({ id: 'file-partial' }),
      delete: async (id) => deleted.push(['file', id]),
    },
    sessions: {
      create: async () => {
        creates += 1;
        if (creates === 2) throw new Error('injected concurrent failure');
        return { id: 'session-partial' };
      },
      delete: async (id) => deleted.push(['session', id]),
    },
  } };
  await assert.rejects(
    () => prepareRecovery({
      client,
      tunnelClient: { beta: { tunnels: {} } },
      toFile: async () => ({}),
      agent: 'agent',
      environmentId: 'environment',
      marker: 'failure',
    }),
    /concurrent recovery prepare failed/u,
  );
  assert.deepEqual(new Set(deleted.map(([kind]) => kind)), new Set(['session', 'file']));
});

test('failed concurrent prepare preserves both primary and compensation failures', async () => {
  // FMECA composition: one Session create succeeds, its concurrent peer fails,
  // then both cleanup arms fail. The outer AggregateError must retain the
  // prepare aggregate and the compensation aggregate in causal order; hiding
  // either would make a release failure or leaked staging resource invisible.
  let creates = 0;
  const client = { beta: {
    files: {
      upload: async () => ({ id: 'file-partial' }),
      delete: async () => { throw new Error('file cleanup failure'); },
    },
    sessions: {
      create: async () => {
        creates += 1;
        if (creates === 2) throw new Error('create failure');
        return { id: 'session-partial' };
      },
      delete: async () => { throw new Error('session cleanup failure'); },
    },
  } };
  await assert.rejects(
    () => prepareRecovery({
      client,
      tunnelClient: { beta: { tunnels: {} } },
      toFile: async () => ({}),
      agent: 'agent',
      environmentId: 'environment',
      marker: 'dual-failure',
    }),
    (error) => error instanceof AggregateError
      && error.errors[0] instanceof AggregateError
      && error.errors[0].errors[0].message === 'create failure'
      && error.errors[1] instanceof AggregateError
      && error.errors[1].errors.map(({ message }) => message).sort().join(',')
        === 'file cleanup failure,session cleanup failure',
  );
});

test('Tunnel prepare failure compensates the Session, File, and active Tunnel', async () => {
  // FMECA: Session/File commits succeed, Tunnel create succeeds, then token
  // rotation fails. The release harness must archive the capability and remove
  // both ordinary resources; otherwise a failed qualification leaks an active
  // ingress credential or leaves staging state that can satisfy a later run.
  const compensated = [];
  const client = { beta: {
    files: {
      upload: async () => ({ id: 'file-tunnel-failure' }),
      delete: async (id) => compensated.push(['file', id]),
    },
    sessions: {
      create: async () => ({ id: 'session-tunnel-failure' }),
      delete: async (id) => compensated.push(['session', id]),
    },
  } };
  const tunnelClient = { beta: { tunnels: {
    create: async () => ({ id: 'tunnel-failure' }),
    rotateToken: async () => { throw new Error('rotation failure'); },
    archive: async (id) => compensated.push(['tunnel', id]),
  } } };
  await assert.rejects(
    () => prepareRecovery({
      client,
      tunnelClient,
      toFile: async () => ({}),
      agent: 'agent',
      environmentId: 'environment',
      marker: 'tunnel-failure',
    }),
    /rotation failure/u,
  );
  assert.deepEqual(new Set(compensated.map(([kind]) => kind)), new Set([
    'session',
    'file',
    'tunnel',
  ]));
});

test('recovery evidence is bound to the exact admitted SDK', () => {
  // Cause/effect graph: C1 prepare records an exact package role+version; C2
  // verify/cleanup run with that same selection. C1+C2 succeeds. A promoted,
  // downgraded, absent, or differently admitted candidate fails before any
  // resource read or cleanup, so two SDKs cannot fabricate one recovery proof.
  const candidate = { role: 'candidate', version: '0.122.0' };
  const identity = recoverySdkIdentity(candidate);
  assert.doesNotThrow(() => assertRecoverySdkIdentity(identity, candidate));
  for (const selected of [
    { role: 'current_oracle', version: '0.121.0' },
    { role: 'candidate', version: '0.123.0' },
  ]) {
    assert.throws(() => assertRecoverySdkIdentity(identity, selected), /one exact SDK/u);
  }
});

function completeRecoveryState() {
  return {
    schema_version: 1,
    marker: 'marker-1',
    command_key: 'managed-recovery-marker-1',
    session_id: 'session-1',
    file_id: 'file-1',
    tunnel_id: 'tunnel-1',
    tunnel_token_sha256: 'a'.repeat(64),
    agent: 'agent-1',
    environment_id: 'environment-1',
    sdk_version: '0.122.0',
    sdk_role: 'candidate',
  };
}

test('recovery state has one closed, atomic, private wire format', () => {
  // Grammar/commit table: the exact eleven-field v1 record is admitted; missing,
  // extra, blank, wrong-version, or wrong-role states fail before any service
  // read. Persistence publishes the whole 0600 file by an atomic no-replace
  // hard link and refuses to
  // overwrite prior evidence, so a partial/stale phase cannot impersonate it.
  const state = completeRecoveryState();
  assert.deepEqual(validateRecoveryState(state), state);
  const mutations = [
    [(value) => { delete value.file_id; }, /fields/u],
    [(value) => { value.extra = true; }, /fields/u],
    [(value) => { value.marker = ' '; }, /marker/u],
    [(value) => { value.sdk_version = 'latest'; }, /SDK version/u],
    [(value) => { value.sdk_role = 'oldest_supported'; }, /SDK role/u],
    [(value) => { value.tunnel_token_sha256 = 'raw-secret'; }, /SHA-256 witness/u],
  ];
  for (const [mutate, pattern] of mutations) {
    const invalid = structuredClone(state);
    mutate(invalid);
    assert.throws(() => validateRecoveryState(invalid), pattern);
  }

  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-recovery-state-'));
  const stateFile = path.join(directory, 'state.json');
  try {
    writeRecoveryState(stateFile, state);
    assert.deepEqual(JSON.parse(fs.readFileSync(stateFile, 'utf8')), state);
    assert.equal(fs.statSync(stateFile).mode & 0o777, 0o600);
    assert.throws(() => writeRecoveryState(stateFile, state), /must be new/u);
  } finally {
    fs.rmSync(directory, { recursive: true, force: true });
  }
});

test('failed recovery-state commit compensates prepared resources without hiding failures', async () => {
  // FMECA: resource prepare commits, then local evidence write fails. Both
  // Session, File, and Tunnel are removed before the write error escapes. If compensation
  // also fails, AggregateError retains the primary and cleanup causes.
  const { calls, client } = recoveryFake();
  const state = completeRecoveryState();
  await assert.rejects(
    () => persistPreparedRecovery({
      client,
      tunnelClient: client,
      stateFile: '/not-written',
      state,
      write: () => { throw new Error('write failure'); },
    }),
    /write failure/u,
  );
  assert.deepEqual(calls, [
    ['session.delete', 'session-1'],
    ['file.delete', 'file-1'],
    ['tunnel.archive', 'tunnel-1'],
  ]);

  const failingCleanup = { beta: {
    sessions: { delete: async () => { throw new Error('session cleanup failure'); } },
    files: { delete: async () => { throw new Error('file cleanup failure'); } },
  } };
  const failingTunnelCleanup = { beta: {
    tunnels: { archive: async () => { throw new Error('tunnel cleanup failure'); } },
  } };
  await assert.rejects(
    () => persistPreparedRecovery({
      client: failingCleanup,
      tunnelClient: failingTunnelCleanup,
      stateFile: '/not-written',
      state,
      write: () => { throw new Error('write failure'); },
    }),
    (error) => error instanceof AggregateError
      && error.errors[0].message === 'write failure'
      && error.errors[1] instanceof AggregateError,
  );
});
