import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';

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

function camelCase(segment) {
  return segment.replace(/_([a-z])/gu, (_, letter) => letter.toUpperCase());
}

function canonicalPythonID(id) {
  return id
    .split('.')
    .map(camelCase)
    .join('.')
    .replace(/createEnrollmentUrl$/u, 'createEnrollmentURL')
    .replace(/mcpOauthValidate$/u, 'mcpOAuthValidate');
}

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
  const pythonIDs = python.current.operations.map(({ id }) => canonicalPythonID(id));
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
    (operation) => [canonicalPythonID(operation.id), operation],
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
