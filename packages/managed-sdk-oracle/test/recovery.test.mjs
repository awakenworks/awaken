import assert from 'node:assert/strict';
import test from 'node:test';

import { cleanupRecovery, prepareRecovery, verifyRecovery } from '../src/conformance/recovery.mjs';

function page(values) {
  return { async *[Symbol.asyncIterator]() { yield* values; } };
}

function recoveryFake() {
  const calls = [];
  const state = { session: null, file: { id: 'file-1' } };
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
    toFile: async (value, name) => ({ value, name }),
    agent: 'agent-1',
    environmentId: 'environment-1',
    marker: 'marker-1',
  });
  assert.equal(calls.filter(([name]) => name === 'session.create').length, 2, 'C1/E1');
  await verifyRecovery({ client, state });
  await cleanupRecovery({ client, state });
  assert.deepEqual(calls.slice(-2), [
    ['session.delete', 'session-1'],
    ['file.delete', 'file-1'],
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
      toFile: async () => ({}),
      agent: 'agent',
      environmentId: 'environment',
      marker: 'failure',
    }),
    /concurrent recovery prepare failed/u,
  );
  assert.deepEqual(new Set(deleted.map(([kind]) => kind)), new Set(['session', 'file']));
});
