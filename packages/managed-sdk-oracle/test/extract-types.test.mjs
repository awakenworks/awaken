import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  managedTypeFingerprint,
  managedTypeFingerprintFromPackageRoot,
  managedWireContract,
} from '../src/extract-types.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('managed declaration fingerprint is deterministic and scoped', () => {
  // Cause/effect graph: C1 exact installed SDK declarations and C2 reviewed
  // Managed resource scope produce E1 one deterministic fingerprint. A package
  // upgrade or scoped type change produces E2 a different review artifact;
  // unrelated Beta Admin declarations never enter the hash.
  const first = managedTypeFingerprint('@anthropic-ai/sdk-current', scope);
  const second = managedTypeFingerprint('@anthropic-ai/sdk-current', scope);
  assert.deepEqual(first, second, 'C1+C2/E1');
  assert.match(first.fingerprint, /^[0-9a-f]{64}$/);
  assert.ok(first.file_count > scope.beta_resource_roots.length);
  assert.ok(
    first.files.some(({ path: declaration }) => declaration === 'beta/webhooks.d.ts'),
    'non-HTTP Webhook helpers are part of the type oracle',
  );
  assert.deepEqual(
    managedTypeFingerprintFromPackageRoot(
      resolveSdkPackage('@anthropic-ai/sdk-current').root,
      scope,
    ),
    first,
    'an explicitly provisioned candidate uses the same fingerprint authority',
  );
  assert.throws(
    () => managedTypeFingerprintFromPackageRoot(packageRoot, scope),
    /is not an @anthropic-ai\/sdk package root/u,
  );
});

test('current wire contract extracts recursive Session shape and event vocabulary from declarations', () => {
  // Cause/effect graph: C1 one exact current SDK package contains the Session
  // and nested SessionAgent interfaces plus event discriminators; C2 properties
  // are required/optional; C3 event literals are inbound/outbound/preview.
  // Effects: E1 generate one recursively closed Session property contract and
  // event catalog, including required-nullable Agent properties; E2 an
  // absent/ambiguous declaration fails extraction. Decision rules:
  // W1=C1+C2+C3=>E1; W2=!C1 or ambiguous interface=>E2. No consumer owns
  // another property/event list.
  const wire = managedWireContract('@anthropic-ai/sdk-current');
  assert.ok(wire.session.required.includes('id'));
  assert.ok(wire.session.required.includes('status'));
  assert.ok(wire.session.optional.includes('deployment_id'));
  assert.ok(wire.session.nested.agent.required.includes('multiagent'));
  assert.equal(wire.session.nested.agent.optional.includes('multiagent'), false);
  assert.equal(wire.session.required.includes('preparation'), false);
  assert.equal(wire.session.optional.includes('preparation'), false);
  for (const shape of [wire.session, wire.session.nested.agent]) {
    assert.equal(
      new Set([...shape.required, ...shape.optional]).size,
      shape.required.length + shape.optional.length,
      'required and optional properties are disjoint at every extracted object',
    );
  }
  assert.ok(wire.events.outbound.includes('session.status_idle'));
  assert.ok(wire.events.inbound.includes('user.message'));
  assert.deepEqual(wire.events.preview, ['event_delta', 'event_start']);
  assert.ok(wire.managed_betas.includes('managed-agents-2026-04-01'));
});
