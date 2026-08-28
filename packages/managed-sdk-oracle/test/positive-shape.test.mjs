import assert from 'node:assert/strict';
import test from 'node:test';

import {
  compareManagedSessionResponseKeyShapes,
  managedSessionResponseKeyShape,
} from '../src/conformance/positive-shape.mjs';

function session() {
  return {
    id: 'sesn_fixture',
    type: 'session',
    agent: {
      id: 'agent_fixture',
      type: 'agent',
      name: 'Fixture',
    },
    status: 'idle',
    stats: { created_at: 'now' },
    usage: { input_tokens: 0 },
  };
}

test('positive Session differential rejects every extra or missing owned field', () => {
  // Causal graph: C1 the same exact SDK decodes an Awaken and official-reference
  // success; C2 their dynamic identities and values may differ; C3 field
  // ownership must not. Effects: E1 value-only changes preserve the key shape;
  // E2 any extra/missing Session or stable nested DTO field fails. This closes
  // the gap where two independent positive lifecycles both passed while one
  // product silently emitted an out-of-contract extension.
  const reference = managedSessionResponseKeyShape(session());
  const differentValues = session();
  differentValues.id = 'sesn_other';
  differentValues.status = 'running';
  compareManagedSessionResponseKeyShapes(
    managedSessionResponseKeyShape(differentValues),
    reference,
    'value non-interference',
  );

  for (const mutate of [
    (value) => { value.preparation = { status: 'ready' }; },
    (value) => { delete value.status; },
    (value) => { value.agent.extension = true; },
    (value) => { value.stats.extension = true; },
    (value) => { value.usage.extension = true; },
  ]) {
    const changed = session();
    mutate(changed);
    assert.throws(
      () => compareManagedSessionResponseKeyShapes(
        managedSessionResponseKeyShape(changed),
        reference,
        'field drift',
      ),
      /successful Session response field shape/u,
      'E2',
    );
  }
});
