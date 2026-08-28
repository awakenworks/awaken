import assert from 'node:assert/strict';

function objectKeys(value, label) {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value), `${label} is an object`);
  return Object.keys(value).sort();
}

export function managedSessionResponseKeyShape(session) {
  return Object.freeze({
    session: objectKeys(session, 'Session'),
    agent: objectKeys(session.agent, 'Session.agent'),
    stats: objectKeys(session.stats, 'Session.stats'),
    usage: objectKeys(session.usage, 'Session.usage'),
  });
}

export function compareManagedSessionResponseKeyShapes(actual, reference, label) {
  assert.deepEqual(actual, reference, `${label}: successful Session response field shape`);
}
