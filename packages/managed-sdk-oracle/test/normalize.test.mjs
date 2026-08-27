import assert from 'node:assert/strict';
import test from 'node:test';

import { normalizeRoute, operationNamespace, stableJson } from '../src/normalize.mjs';

test('route normalization removes transport selectors and parameter spelling', () => {
  // Cause/effect graph: C1 beta query, C2 SDK template parameter, C3 Axum
  // wildcard spelling. Effect E1 one method/path identity independent of SDK
  // generator and server-router syntax.
  assert.equal(normalizeRoute('/v1/sessions/${sessionID}?beta=true'), '/v1/sessions/{}');
  assert.equal(normalizeRoute('/v1/models/{*id}'), '/v1/models/{}');
});

test('operation namespaces are stable across flat and nested generated modules', () => {
  assert.equal(operationNamespace('/sdk/resources/beta', '/sdk/resources/beta/files.js'), 'beta.files');
  assert.equal(
    operationNamespace('/sdk/resources/beta', '/sdk/resources/beta/sessions/sessions.js'),
    'beta.sessions',
  );
  assert.equal(
    operationNamespace('/sdk/resources/beta', '/sdk/resources/beta/sessions/threads/events.js'),
    'beta.sessions.threads.events',
  );
});

test('stable JSON sorts object keys without reordering semantic arrays', () => {
  assert.deepEqual(stableJson({ z: 1, a: { y: 2, b: 3 }, list: [2, 1] }), {
    a: { b: 3, y: 2 },
    list: [2, 1],
    z: 1,
  });
});
