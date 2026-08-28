import assert from 'node:assert/strict';
import test from 'node:test';

import { assertLatestRuntimeOwnsCandidateDelta } from './official_sdk_candidate_delta.mjs';

const empty = () => ({ added: [], removed: [], changed: [] });

test('candidate delta ownership fails closed outside exercised change points', () => {
  // Cause/effect graph: C1 no operation is added/removed; C2 only existing Beta
  // Files/Skills request signatures change; C3 only Files/Skills/Webhooks
  // declaration contents change in place; C4 any other delta. Effects: E1 reuse
  // the generic candidate runtime proof; E2 require a new explicit owner.
  // Decision rule D1 C1+C2+C3->E1; D2 C4->E2. This prevents a green smoke of
  // unchanged calls from certifying a newly added or unrelated SDK capability.
  const owned = {
    operations: {
      ...empty(),
      changed: [{ id: 'beta.files.list' }, { id: 'beta.skills.create' }],
    },
    declarations: {
      ...empty(),
      changed: [
        { path: 'beta/files.d.ts' },
        { path: 'beta/skills/versions.d.ts' },
        { path: 'beta/webhooks.d.ts' },
      ],
    },
    runtime: {
      ...empty(),
      changed: [
        { path: 'core/middleware.mjs' },
        { path: 'internal/uploads.mjs' },
        { path: 'lib/sessions/accumulate.mjs' },
        { path: 'resources/beta/files.mjs' },
        { path: 'resources/beta/skills/versions.mjs' },
        { path: 'resources/beta/webhooks.mjs' },
        { path: 'tools/agent-toolset/node.mjs' },
        { path: 'version.mjs' },
      ],
    },
  };
  assert.doesNotThrow(() => assertLatestRuntimeOwnsCandidateDelta(owned), 'D1/E1');

  const rejected = [
    { ...owned, operations: { ...empty(), added: [{ id: 'beta.files.share' }] } },
    { ...owned, operations: { ...empty(), changed: [{ id: 'beta.sessions.create' }] } },
    { ...owned, declarations: { ...empty(), added: [{ path: 'beta/files/new.d.ts' }] } },
    { ...owned, declarations: { ...empty(), changed: [{ path: 'beta/sessions.d.ts' }] } },
    { ...owned, runtime: { ...empty(), added: [{ path: 'lib/new-helper.mjs' }] } },
    { ...owned, runtime: { ...empty(), changed: [{ path: 'lib/tools/SessionToolRunner.mjs' }] } },
  ];
  for (const delta of rejected) {
    assert.throws(() => assertLatestRuntimeOwnsCandidateDelta(delta), undefined, 'D2/E2');
  }
});
