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

test('every official TypeScript Managed SDK method has executable method-level evidence', () => {
  const beta = new Anthropic({ apiKey: 'surface-inventory' }).beta; // awaken-allow: secret
  const actual = publicMethods(beta)
    .filter((method) => !method.startsWith('messages.'))
    .sort();
  const expected = MANAGED_TS_METHOD_MANIFEST.map((entry) => entry.sdkMethod).sort();
  assert.deepEqual(expected, actual, 'new/removed SDK methods require an explicit manifest decision');
  assert.equal(new Set(expected).size, expected.length, 'each SDK method appears exactly once');

  for (const evidence of MANAGED_TS_METHOD_MANIFEST) {
    assert.ok(evidence.route, `${evidence.sdkMethod}: route`);
    for (const axis of ['happy', 'missing', 'invalidState', 'authWorkspace']) {
      assert.ok(evidence[axis], `${evidence.sdkMethod}: ${axis}`);
    }
    const source = readFileSync(resolve(E2E, evidence.owner), 'utf8').replace(/\s+/g, '');
    assert.ok(
      source.includes(`.beta.${evidence.sdkMethod}(`),
      `${evidence.sdkMethod}: ${evidence.owner} does not actually invoke the official SDK method`,
    );
  }
});
