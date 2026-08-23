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

test('every official TypeScript Managed SDK method has one executable owner', () => {
  // Cause/effect graph: C1=runtime SDK methods exactly match the manifest;
  // C2=each method appears once with a route; C3=its named E2E owner calls that
  // official method directly or invokes one explicit canonical helper that
  // calls it; C4=the owner is reachable from the deterministic graph.
  // Effect E1=the SDK surface has one executable owner. Decision rule R1:
  // C1 && C2 && C3 && C4 => E1; any false condition fails with that missing
  // ownership edge instead of accepting a parallel or unexecuted evidence table.
  // Constraints/invariant: one SDK method maps to exactly one route/owner entry;
  // package orchestration, not a second manifest, proves reachability.
  const beta = new Anthropic({ apiKey: 'surface-inventory' }).beta; // awaken-allow: secret
  const actual = publicMethods(beta)
    .filter((method) => !method.startsWith('messages.'))
    .sort();
  const expected = MANAGED_TS_METHOD_MANIFEST.map((entry) => entry.sdkMethod).sort();
  assert.deepEqual(expected, actual, 'new/removed SDK methods require an explicit manifest decision');
  assert.equal(new Set(expected).size, expected.length, 'each SDK method appears exactly once');

  const orchestration = [
    readFileSync(resolve(E2E, 'package.json'), 'utf8'),
    readFileSync(resolve(E2E, 'stage_change_coverage_e2e.ts'), 'utf8'),
  ].join('\n');

  for (const evidence of MANAGED_TS_METHOD_MANIFEST) {
    assert.ok(evidence.route, `${evidence.sdkMethod}: route`);
    const source = readFileSync(resolve(E2E, evidence.owner), 'utf8').replace(/\s+/g, '');
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
          && helperSource.includes(`.beta.${evidence.sdkMethod}(`),
        `${evidence.sdkMethod}: ${evidence.sdkHelper} does not invoke the official SDK method`,
      );
    } else {
      assert.ok(
        source.includes(`.beta.${evidence.sdkMethod}(`),
        `${evidence.sdkMethod}: ${evidence.owner} does not invoke the official SDK method`,
      );
    }
    assert.ok(
      orchestration.includes(evidence.owner),
      `${evidence.sdkMethod}: ${evidence.owner} is not in the deterministic execution graph`,
    );
  }
});
