import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

import {
  managedTypeFingerprint,
  managedTypeFingerprintFromPackageRoot,
  managedDeclaredTypes,
  managedWireContract,
} from '../src/extract-types.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const scope = JSON.parse(fs.readFileSync(path.join(packageRoot, 'config/scope.json')));

test('one declaration fingerprint rejects divergent ESM and CommonJS projections', () => {
  // Cause/effect rules: D1 paired import/export/import-type specifiers differ
  // only by .js/.mjs and maps by .d.ts/.d.mts => one canonical fingerprint;
  // D2 CRLF/LF => the SAME fingerprint; D3 ESM field type changes, D4 a module
  // target changes, D5 a field string literal changes, D6 docs change, or D7 the
  // ESM counterpart disappears => fail before admitting candidate declarations.
  // Constraint: never normalize arbitrary strings or keep a second fingerprint
  // ledger for the same wire types. Each mutation resets the same exact fixture.
  const root = fs.mkdtempSync(path.join(tmpdir(), 'awaken-declaration-pair-'));
  try {
    fs.writeFileSync(path.join(root, 'package.json'), JSON.stringify({ name: '@anthropic-ai/sdk', version: '0.128.0' }));
    const directory = path.join(root, 'resources/beta/sessions');
    fs.mkdirSync(directory, { recursive: true });
    const file = path.join(directory, 'events.d.ts');
    const twin = path.join(directory, 'events.d.mts');
    const cjs = "import { X } from './peer.js';\nexport type { X } from './peer.js';\nexport type Y = import('./peer.js').X;\nexport type Permission = 'allow';\nexport type Literal = 'value.mjs';\n// docs: data.mjs\n//# sourceMappingURL=events.d.ts.map\n";
    const esm = cjs.replaceAll('./peer.js', './peer.mjs').replace('events.d.ts.map', 'events.d.mts.map');
    const selectedScope = { beta_resource_roots: ['sessions'] };
    fs.writeFileSync(file, cjs);
    fs.writeFileSync(twin, esm);
    const expected = managedTypeFingerprintFromPackageRoot(root, selectedScope);
    assert.equal(expected.file_count, 1, 'D1 one canonical coordinate');
    fs.writeFileSync(twin, esm.replaceAll('\n', '\r\n'));
    assert.deepEqual(managedTypeFingerprintFromPackageRoot(root, selectedScope), expected, 'D2');
    for (const [rule, before, after] of [
      ['D3', "Permission = 'allow'", "Permission = 'allow' | 'deny'"],
      ['D4', './peer.mjs', './other.mjs'],
      ['D5', "Literal = 'value.mjs'", "Literal = 'value.js'"],
      ['D6', '// docs: data.mjs', '// docs: data.js'],
    ]) {
      fs.writeFileSync(twin, esm.replace(before, after));
      assert.throws(() => managedTypeFingerprintFromPackageRoot(root, selectedScope), /declarations disagree/u, rule);
    }
    fs.rmSync(twin);
    assert.throws(() => managedTypeFingerprintFromPackageRoot(root, selectedScope), /missing ESM declaration/u, 'D7');
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

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
  for (const declaration of [
    'managed/helpers/beta/zod.d.ts',
    'managed/lib/environments/worker.d.ts',
    'managed/lib/sessions/accumulate.d.ts',
    'managed/tools/agent-toolset/node.d.ts',
    'managed/tools/agent-toolset/memories.d.ts',
    'managed/tools/agent-toolset/skills.d.ts',
  ]) {
    assert.ok(
      first.files.some(({ path: candidate }) => candidate === declaration),
      `Managed helper declaration ${declaration} is fingerprinted`,
    );
  }
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
  assert.deepEqual(wire.resource_params.memory_store.required, ['memory_store_id', 'type']);
  assert.deepEqual(wire.resource_params.memory_store.optional, ['access', 'instructions']);
  assert.ok(wire.resource_params.file.optional.includes('mount_path'));
  // 0.125: the GitHub token is required only for private repositories, so the
  // declaration moved it to optional; a public repository mounts without one.
  assert.ok(wire.resource_params.github_repository.optional.includes('authorization_token'));
  assert.equal(wire.resource_params.github_repository.required.includes('authorization_token'), false);
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
  // Pagination is declared once in the SDK core and shared by every Managed
  // collection. The cursor envelope's continuation is required-nullable
  // (`next_page: string | null`), so a last page must still send the key;
  // both query parameters are optional.
  assert.deepEqual(wire.pagination.PageCursorResponse, {
    required: ['data', 'next_page'],
    optional: [],
  });
  assert.deepEqual(wire.pagination.PageResponse, {
    required: ['data', 'first_id', 'has_more', 'last_id'],
    optional: [],
  });
  assert.deepEqual(wire.pagination.PageCursorParams, {
    required: [],
    optional: ['limit', 'page'],
  });
});

test('current wire contract describes every committed event down to its nested discriminated fields', () => {
  // Cause/effect graph: C1 every member of the committed event union carries
  // its own property list; C2 a property may itself be a discriminated object
  // union (0.125's `evaluation`, and its `evaluated_permission` under `auto`).
  // Effects: E1 each outbound event type has a shape with disjoint
  // required/optional names; E2 a nested discriminated field is described per
  // variant, two levels deep; E3 an array-of-blocks field is not mistaken for
  // a discriminated object. Decision rules: S1=C1=>E1; S2=C1+C2=>E2;
  // S3=content arrays=>E3. This is what makes a field added to an existing
  // event a visible contract change rather than a fingerprint delta nobody
  // can read.
  const wire = managedWireContract('@anthropic-ai/sdk-current');
  for (const type of wire.events.outbound) {
    const shape = wire.events.shapes[type];
    assert.ok(shape, `S1 ${type} has a shape`);
    assert.equal(
      new Set([...shape.required, ...shape.optional]).size,
      shape.required.length + shape.optional.length,
      `S1 ${type} required and optional are disjoint`,
    );
    assert.ok(shape.required.includes('type'), `S1 ${type} carries its discriminator`);
  }
  const toolUse = wire.events.shapes['agent.tool_use'];
  assert.ok(toolUse.optional.includes('evaluated_permission'), 'S1');
  assert.ok(toolUse.optional.includes('evaluation'), 'S2');
  assert.deepEqual(Object.keys(toolUse.nested.evaluation).sort(), ['always_allow', 'always_ask', 'auto'], 'S2');
  const auto = toolUse.nested.evaluation.auto;
  assert.deepEqual(auto.required, ['evaluated_permission', 'type'], 'S2');
  assert.deepEqual(Object.keys(auto.nested.evaluated_permission).sort(), ['allow', 'ask', 'deny'], 'S2 two levels deep');
  assert.deepEqual(auto.nested.evaluated_permission.deny.required, ['reason_code', 'type'], 'S2');
  assert.deepEqual(auto.nested.evaluated_permission.allow.required, ['type'], 'S2');
  assert.deepEqual(wire.events.shapes['agent.mcp_tool_use'].nested.evaluation, toolUse.nested.evaluation, 'S2 same object on both tool-use events');
  assert.equal(wire.events.shapes['agent.message'].nested, undefined, 'S3 content blocks are not a discriminated field');
  assert.ok(wire.events.shapes['span.outcome_evaluation_end'].required.includes('usage'), 'S1 a required field the adapter must carry');
});

test('the current SDK declared-type vocabulary is extracted for the wire naming check', () => {
  // Cause: C1 every scoped Managed declaration file exports named types.
  // Effect: E1 one sorted list of those names, which the Rust wire types cite
  // as their exact SDK counterparts. The names introduced by 0.125 are the
  // witnesses that the vocabulary reads the events declarations, not only the
  // resource roots.
  const names = managedDeclaredTypes('@anthropic-ai/sdk-current', scope);
  assert.deepEqual(names, [...new Set(names)].sort(), 'E1 sorted and unique');
  for (const witness of [
    'BetaManagedAgentsSession',
    'BetaManagedAgentsAgentToolUseEvent',
    'BetaManagedAgentsAgentToolEvaluation',
    'BetaManagedAgentsAgentAutoEvaluatedPermissionDeny',
    'BetaManagedAgentsAutoPolicy',
    'BetaFileMetadata',
    // The shared pagination vocabulary lives in `core/pagination.d.ts`, not
    // under a resource root; these witness that it is in scope.
    'PageCursor',
    'PageCursorParams',
    'Page',
  ]) {
    assert.ok(names.includes(witness), `E1 ${witness}`);
  }
});
