import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

import { canonicalPythonOperationID } from '../src/python-operation-identity.mjs';

const REPO = resolve(import.meta.dirname, '../../..');
const python = JSON.parse(readFileSync(resolve(
  REPO,
  'contracts/anthropic-managed/python-upstream-oracle.generated.json',
), 'utf8'));
const typescript = JSON.parse(readFileSync(resolve(
  REPO,
  'contracts/anthropic-managed/upstream-oracle.generated.json',
), 'utf8'));
const candidateQualifications = JSON.parse(readFileSync(resolve(
  REPO,
  'e2e/conformance/official_sdk_candidate_qualifications.json',
), 'utf8'));
const pythonRuntime = [
  readFileSync(resolve(REPO, 'e2e/conformance/managed_python_sdk_runtime_e2e.py'), 'utf8'),
  readFileSync(resolve(REPO, 'e2e/conformance/managed_python_sdk_helpers_e2e.py'), 'utf8'),
].join('\n');

function withoutID(operation) {
  const { id: _, ...coordinate } = operation;
  return coordinate;
}

test('Python current SDK has the exact TypeScript Managed operation identity set', () => {
  // Cross-language cause/effect graph: C1=the official Python 1.2 wheel yields
  // one normalized operation id; C2=the reviewed TypeScript oracle yields the
  // corresponding camelCase id; C3=the two language generators use different
  // acronym conventions only for URL/OAuth. Effects: E1=all 127 Managed calls
  // share one protocol identity set; E2=a new, removed, or ambiguously renamed
  // operation fails closed. Decision table: C1+C2+(C3 when applicable)=>E1;
  // any missing/duplicate mapping=>E2. This test deliberately reuses the
  // canonical TS ledger rather than introducing a second behavior-owner table.
  const pythonIDs = python.current.operations.map(({ id }) => canonicalPythonOperationID(id));
  const typescriptIDs = typescript.current.operations.map(({ id }) => id);
  assert.equal(new Set(pythonIDs).size, pythonIDs.length, 'Python ids canonicalize injectively');
  assert.deepEqual(pythonIDs.sort(), typescriptIDs.sort());
  assert.equal(pythonIDs.length, 127);
});

test('Python 1.2 beta-to-GA drift is exactly the reviewed TypeScript 0.122 delta', () => {
  // Selector cause/effect graph: C1=Python 1.2 removed dated selector pins from
  // beta Files/Skills while retaining `beta=true`; C2=TypeScript 0.121 is the
  // last pinned oracle; C3=the exact 0.122 candidate qualification owns every
  // changed operation. Effects: E1=method/path/query stay identical and only
  // the 14 reviewed beta-header coordinates differ; E2=any unreviewed
  // cross-language drift fails. Decision table: C1+C2+C3=>E1; a changed route,
  // verb, query, extra selector, or absent qualification=>E2.
  const pythonByID = new Map(python.current.operations.map(
    (operation) => [canonicalPythonOperationID(operation.id), operation],
  ));
  const differences = [];
  for (const operation of typescript.current.operations) {
    const pythonOperation = pythonByID.get(operation.id);
    assert.ok(pythonOperation, `${operation.id}: Python operation`);
    if (JSON.stringify(withoutID(pythonOperation)) !== JSON.stringify(withoutID(operation))) {
      differences.push(operation.id);
      assert.deepEqual(
        {
          method: pythonOperation.method,
          path: pythonOperation.path,
          transport_query: pythonOperation.transport_query,
        },
        {
          method: operation.method,
          path: operation.path,
          transport_query: operation.transport_query,
        },
        `${operation.id}: transport coordinate`,
      );
      assert.deepEqual(pythonOperation.betas, [], `${operation.id}: Python 1.2 selector removal`);
      assert.ok(operation.betas.length > 0, `${operation.id}: prior selector pin`);
    }
  }
  const reviewed = candidateQualifications.qualifications
    .flatMap(({ evidence_groups: evidenceGroups }) => evidenceGroups)
    .flatMap(({ coordinates }) => coordinates)
    .filter((coordinate) => coordinate.startsWith('operations:changed:'))
    .map((coordinate) => coordinate.replace('operations:changed:', ''))
    .sort();
  assert.deepEqual(differences.sort(), reviewed);
  assert.equal(differences.length, 14);
});

test('every handwritten Python Managed helper is oracle-visible and runtime-owned', () => {
  // Helper coverage graph: C1=AST extraction separates handwritten helpers
  // from generated HTTP operations/subresource properties; C2=the current
  // wheel exposes five helpers; C3=one real-runtime driver names each public
  // entrypoint. Effects: E1=helper addition/removal changes the wheel-bound
  // oracle; E2=no helper can sit outside executable coverage. Decision table:
  // C1+C2+C3=>E1+E2; missing identity, duplicate, stale fingerprint, or absent
  // runtime owner=>reject.
  const expected = [
    'beta.environments.work.poller',
    'beta.environments.work.worker',
    'beta.sessions.events.tool_runner',
    'beta.webhooks.parse_unverified',
    'beta.webhooks.unwrap',
  ];
  assert.deepEqual(python.current.helpers, expected);
  assert.match(python.current.helper_fingerprint, /^[0-9a-f]{64}$/u);
  for (const helper of expected) {
    const method = helper.split('.').at(-1);
    assert.match(pythonRuntime, new RegExp(`\\.${method}\\(`, 'u'), `${helper}: runtime owner`);
  }
  for (const anchor of python.anchors) {
    assert.equal(anchor.helper_count, anchor.helpers.length, `${anchor.version}: helper count`);
    assert.match(anchor.helper_fingerprint, /^[0-9a-f]{64}$/u, `${anchor.version}: helper fingerprint`);
    assert.equal(
      anchor.library_export_count,
      anchor.library_exports.length,
      `${anchor.version}: library export count`,
    );
    assert.match(
      anchor.library_export_fingerprint,
      /^[0-9a-f]{64}$/u,
      `${anchor.version}: library export fingerprint`,
    );
  }
});

test('Python handwritten Managed library modules have one exact import surface', () => {
  // Import-surface graph: C1=three reviewed handwritten modules are in scope;
  // C2=their explicit __all__ declarations are extracted from the exact wheel;
  // C3=the isolated runtime driver imports the generated identities. Effects:
  // E1=29 current symbols remain visible without hand-maintained stubs; E2=a
  // module removal/addition or moved symbol requires explicit oracle review.
  // Constants/types receive import coverage here; executable owners remain the
  // accumulator/toolset/poller/runner/worker causal scenarios.
  const groups = new Map();
  for (const identity of python.current.library_exports) {
    const module = identity.split('.').slice(0, -1).join('.');
    groups.set(module, [...(groups.get(module) ?? []), identity]);
  }
  assert.deepEqual([...groups.keys()].sort(), [
    'anthropic.lib.environments',
    'anthropic.lib.sessions',
    'anthropic.lib.tools.agent_toolset',
  ]);
  assert.deepEqual([...groups.values()].map(({ length }) => length).sort((a, b) => a - b), [2, 11, 16]);
  assert.equal(python.current.library_exports.length, 29);
  assert.match(python.current.library_export_fingerprint, /^[0-9a-f]{64}$/u);
  assert.match(pythonRuntime, /\["current"\]\["library_exports"\]/u);
});

test('Python anchors are monotonic reviewed change points with one exact wheel each', () => {
  // Version-matrix cause/effect graph: C1=0.92 is the first supported Managed
  // release; C2=each later selected release owns a protocol, GA, or transport
  // change; C3=1.2 is the sole current oracle; C4=an official wheel digest and
  // extracted source/operation fingerprints bind every row. Effect E1=the
  // matrix tests behavior-changing versions without redundant patch sampling.
  // Decision table: C1+C2+C3+C4=>E1; missing reason/fingerprint, duplicate
  // wheel, non-monotonic surface, or multiple current rows=>reject.
  assert.equal(python.anchors[0].version, '0.92.0');
  assert.equal(python.anchors.at(-1).version, '1.2.0');
  assert.equal(python.anchors.filter(({ role }) => role === 'current_oracle').length, 1);
  const wheelDigests = new Set();
  let priorCount = 0;
  for (const anchor of python.anchors) {
    assert.ok(anchor.reason.length > 20, `${anchor.version}: reviewed reason`);
    assert.match(anchor.wheel.sha256, /^[0-9a-f]{64}$/u, `${anchor.version}: wheel digest`);
    assert.match(anchor.operation_fingerprint, /^[0-9a-f]{64}$/u);
    assert.match(anchor.source_fingerprint, /^[0-9a-f]{64}$/u);
    assert.ok(anchor.operation_count >= priorCount, `${anchor.version}: additive operation surface`);
    assert.deepEqual(anchor.only_in_anchor, [], `${anchor.version}: no removed Managed operation`);
    wheelDigests.add(anchor.wheel.sha256);
    priorCount = anchor.operation_count;
  }
  assert.equal(wheelDigests.size, python.anchors.length);
});
