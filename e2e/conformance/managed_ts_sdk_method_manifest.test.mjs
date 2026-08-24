import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import Anthropic from '@anthropic-ai/sdk';
import { MANAGED_TS_METHOD_MANIFEST } from './managed_ts_sdk_method_manifest.mjs';

const E2E = resolve(import.meta.dirname, '..');

function publicMethods(resource, prefix = '', depth = 0, found = []) {
  if (!resource || depth > 4) return found;
  for (const name of Object.getOwnPropertyNames(Object.getPrototypeOf(resource) ?? {})) {
    if (name !== 'constructor' && typeof resource[name] === 'function') found.push(`${prefix}${name}`);
  }
  for (const name of Object.keys(resource)) {
    if (name !== '_client' && resource[name] && typeof resource[name] === 'object') {
      publicMethods(resource[name], `${prefix}${name}.`, depth + 1, found);
    }
  }
  return found;
}

function officialManagedMethods(client) {
  return [
    ...publicMethods(client.beta)
      .filter((method) => !method.startsWith('messages.'))
      .map((method) => `beta.${method}`),
    ...publicMethods(client.models, 'models.'),
    ...publicMethods(client.files, 'files.'),
    ...publicMethods(client.skills, 'skills.'),
  ].sort();
}

function assertExactMethodInventory(manifest, actual) {
  const expected = manifest.map((entry) => entry.sdkMethod).sort();
  assert.equal(new Set(expected).size, expected.length, 'each SDK method appears exactly once');
  assert.deepEqual(expected, actual, 'new/removed SDK methods require an explicit manifest decision');
}

test('every official TypeScript Managed SDK method has one executable owner', () => {
  // Cause/effect graph: C1=the Beta Managed methods exactly match the manifest;
  // C2=the GA Models/Files/Skills methods exactly match the same manifest;
  // C3=each root-qualified method appears once with a route; C4=its named E2E
  // owner calls that exact Beta or GA entrypoint directly (or one explicit
  // canonical helper); C5=the owner is reachable from the deterministic graph.
  // Effect E1=the SDK surface has one executable owner. Decision rule R1:
  // C1 && C2 && C3 && C4 && C5 => E1; any false condition fails with that missing
  // ownership edge instead of accepting a parallel or unexecuted evidence table.
  // Constraints/invariant: Beta and GA share one inventory but retain distinct
  // call identities; package orchestration, not a second manifest, proves reachability.
  const client = new Anthropic({ apiKey: 'surface-inventory' }); // awaken-allow: secret
  const actual = officialManagedMethods(client);
  assertExactMethodInventory(MANAGED_TS_METHOD_MANIFEST, actual);

  const orchestration = [
    readFileSync(resolve(E2E, 'package.json'), 'utf8'),
    readFileSync(resolve(E2E, 'stage_change_coverage_e2e.ts'), 'utf8'),
  ].join('\n');

  for (const evidence of MANAGED_TS_METHOD_MANIFEST) {
    assert.ok(evidence.route, `${evidence.sdkMethod}: route`);
    const source = readFileSync(resolve(E2E, evidence.owner), 'utf8').replace(/\s+/g, '');
    const directNeedle = evidence.sdkRoot === 'beta'
      ? `.beta.${evidence.relativeMethod}(`
      : `client.${evidence.relativeMethod}(`;
    if (evidence.sdkHelper) {
      const [helperOwner, helperName] = evidence.sdkHelper.split('#');
      assert.ok(helperOwner && helperName, `${evidence.sdkMethod}: malformed helper evidence`);
      assert.ok(
        source.includes(helperOwner) && source.includes(`${helperName}(`),
        `${evidence.sdkMethod}: ${evidence.owner} does not invoke ${evidence.sdkHelper}`,
      );
      const helperSource = readFileSync(resolve(E2E, helperOwner), 'utf8').replace(/\s+/g, '');
      assert.ok(
        helperSource.includes(`function${helperName}(`)
          && helperSource.includes(directNeedle),
        `${evidence.sdkMethod}: ${evidence.sdkHelper} does not invoke the official SDK method`,
      );
    } else {
      assert.ok(
        source.includes(directNeedle),
        `${evidence.sdkMethod}: ${evidence.owner} does not invoke the official SDK method`,
      );
    }
    assert.ok(
      orchestration.includes(evidence.owner),
      `${evidence.sdkMethod}: ${evidence.owner} is not in the deterministic execution graph`,
    );
  }
});

test('Managed SDK method ownership rejects missing and overlapping scoped entries', () => {
  // Cause/effect graph: C1 one official root-qualified method is missing; C2
  // one is owned twice. Effects: E1 C1 fails exact surface equality; E2 C2
  // fails uniqueness before a duplicate owner can mask drift. Constraint: this
  // mutates copies of the one production manifest, never a second inventory.
  // Decision rules: R2 C1->E1; R3 C2->E2.
  const client = new Anthropic({ apiKey: 'surface-inventory' }); // awaken-allow: secret
  const actual = officialManagedMethods(client);
  assert.throws(
    () => assertExactMethodInventory(MANAGED_TS_METHOD_MANIFEST.slice(1), actual),
    /new\/removed SDK methods/,
    'R2/E1',
  );
  assert.throws(
    () => assertExactMethodInventory(
      [...MANAGED_TS_METHOD_MANIFEST, MANAGED_TS_METHOD_MANIFEST[0]],
      actual,
    ),
    /exactly once/,
    'R3/E2',
  );
});
