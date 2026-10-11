import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import {
  executeLatestCanaryPlan,
  latestCandidateQualification,
  latestCanaryPlan,
} from './sdk_latest_canary_lib.mjs';
import { requestContractInternals } from './managed_sdk_request_contract.mjs';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import { pathToFileURL } from 'node:url';
import { resolve } from 'node:path';
import { exerciseEnvironmentWorkerSecretToken } from './official_environment_worker_contract.mjs';
import { exerciseNonReplayableTransport, exerciseRootSchemaReferences } from './official_sdk_transport_contract.mjs';

test('candidate transport and schema change points reject the old behavior', async () => {
  // T1-T7/S1-S3 are owned by the shared runtime oracle. The pinned historical
  // anchor is the negative control: stream replay and lost root definitions
  // must be rejected, while the exact candidate passes the same observations.
  for (const [module, accepts] of [['@anthropic-ai/sdk-candidate', true], ['@anthropic-ai/sdk-user-profiles-legacy', false]]) {
    const packageRoot = resolveSdkPackage(module).root;
    const { default: Anthropic } = await import(pathToFileURL(resolve(packageRoot, 'index.mjs')));
    if (accepts) {
      await exerciseNonReplayableTransport({ Anthropic, packageRoot });
      await exerciseRootSchemaReferences(packageRoot);
    } else {
      await assert.rejects(exerciseNonReplayableTransport({ Anthropic, packageRoot }), /T1-T4/u);
      await assert.rejects(exerciseRootSchemaReferences(packageRoot), /S1 definitions survive root ref/u);
    }
  }
});

test('worker secret oracle rejects a request made before a matching refusal', async () => {
  // K1 actual pinned Worker + valid token -> exact bearer observed. K2 absent
  // token -> matching refusal AND no extra request. M1 performs a request and
  // then emits the expected refusal; the old self-comparison accepted it.
  // Reuse the canary's sole oracle, so its mutation witness cannot drift from
  // the release consumer. Both invocations use the real SDK transport.
  const root = resolveSdkPackage('@anthropic-ai/sdk-current').root;
  const { default: Anthropic } = await import(pathToFileURL(resolve(root, 'index.mjs')));
  const { EnvironmentWorker } = await import(pathToFileURL(resolve(root, 'lib/environments/worker.mjs')));
  await exerciseEnvironmentWorkerSecretToken({ Anthropic, EnvironmentWorker });
  class RequestBeforeRefusal extends EnvironmentWorker {
    constructor(options) { super(options); this.probeClient = options.client; }
    async handleItem(item) {
      if (JSON.parse(Buffer.from(item.workSecret, 'base64url')).note === 'no token') {
        try { await this.probeClient.models.list(); } catch { /* injected transport failure */ }
        throw new Error('no sessions token could be extracted');
      }
      return super.handleItem(item);
    }
  }
  await assert.rejects(
    exerciseEnvironmentWorkerSecretToken({ Anthropic, EnvironmentWorker: RequestBeforeRefusal }),
    /K2 refusal happened before any request/u,
  );
});

const candidateQualification = Object.freeze({
  baseline_version: '0.120.0',
  candidate_version: '0.121.0',
  module: '@anthropic-ai/sdk-candidate',
  package_integrity: 'sha512-Y2FuZGlkYXRl',
});
const candidateDependencies = Object.freeze({
  '@anthropic-ai/sdk-candidate': 'npm:@anthropic-ai/sdk@0.121.0',
});

test('empty path admission uses actual SDK diagnostics for positional and grouped identities', async () => {
  // C1 empty positional/grouped path id -> actual SDK refusal, zero HTTP,
  // Python ValueError projection; C2 body/query empty without that diagnostic
  // -> no exception; C3 two empty ids -> reject the one-factor witness.
  // The generated SDK owns binding, not a second path-parameter mapper.
  const root = resolveSdkPackage('@anthropic-ai/sdk-current').root;
  const { default: Anthropic } = await import(pathToFileURL(resolve(root, 'index.mjs')));
  let requests = 0;
  const client = new Anthropic({ apiKey: 'path-contract', // awaken-allow(secret):
    maxRetries: 0, fetch: async () => { requests++; throw new Error('unexpected path I/O'); } });
  const classify = requestContractInternals.pythonEmptyPathRejection;
  for (const [invoke, args, field] of [
    [() => client.beta.agents.archive(''), [['agentID', '']], 'agentID'],
    [() => client.beta.environments.work.ack('work', { environment_id: '' }), [['workID', 'work'], ['environment_id', '']], 'environment_id'],
  ]) {
    assert.throws(invoke, (error) => {
      assert.deepEqual(classify(args, error, { id: field }), { error_class: 'ValueError', field }); return true;
    });
  }
  assert.equal(requests, 0, 'C1 zero transport');
  assert.equal(classify([['title', '']], new Error('body rejected'), { id: 'body' }), null, 'C2');
  assert.throws(() => client.beta.environments.work.ack('', { environment_id: '' }), (error) => {
    assert.throws(() => classify([['workID', ''], ['environment_id', '']], error, { id: 'ack' }), /one-factor empty path/u); return true;
  });
});

test('registry latest reuses the generated current oracle only when all versions agree', () => {
  // Cause/effect graph: C1 generated current oracle is exact; C2 installed
  // current.module equals C1; C3 registry latest equals C1; C4 registry drift is
  // younger than pnpm's one release-age policy; C5 drift is mature; C6 any
  // version or policy evidence is invalid. Effects: E1 run the installed oracle;
  // E2 quarantine without installing the candidate; E3 require regeneration;
  // E4 reject stale node_modules; E5 reject ambiguous evidence. Decision table:
  // R1 C1+C2+C3=>E1; R2 C1+C2+!C3+C4=>E1+E2+candidate;
  // R3 C1+C2+!C3+C5=>candidate+E3; R4 C1+!C2=>E4; R5 C6=>E5.
  // Candidate execution is distinct from promotion: release age can defer the
  // latter, never the former, and only a reviewed exact alias may execute.
  assert.deepEqual(latestCanaryPlan('0.121.0', '0.121.0', '0.121.0'), {
    oracle: '0.121.0', latest: '0.121.0', installed: '0.121.0',
  });
});

test('a newer registry version remains quarantined during the dependency observation period', () => {
  // R2: young drift is visible but cannot become executable dependency evidence.
  assert.deepEqual(
    latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
      latestPublishedAt: '2026-08-27T20:35:25.000Z',
      minimumReleaseAgeMinutes: 1_440,
      now: Date.parse('2026-08-27T22:35:25.000Z'),
    }),
    {
      oracle: '0.120.0',
      latest: '0.121.0',
      installed: '0.120.0',
      candidateRequired: true,
      quarantinedUntil: '2026-08-28T20:35:25.000Z',
    },
  );
});

test('a mature newer registry version schedules verification before requiring promotion', async () => {
  // R3 cause/effect graph: C1=release age elapsed, C2=current proof succeeds,
  // C3=candidate proof succeeds. Effects: E1=both exact roots execute in order;
  // E2=the gate then fails and requires oracle promotion. If C3 fails, its
  // original error wins, so stale-oracle reporting cannot mask incompatibility.
  const plan = latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
      latestPublishedAt: '2026-08-27T20:35:25.000Z',
      minimumReleaseAgeMinutes: 1_440,
      now: Date.parse('2026-08-28T20:35:25.000Z'),
  });
  assert.deepEqual(plan, {
    oracle: '0.120.0',
    latest: '0.121.0',
    installed: '0.120.0',
    candidateRequired: true,
    promotionRequired: true,
  });
  const calls = [];
  await assert.rejects(
    executeLatestCanaryPlan(plan, {
      current: () => calls.push('current'),
      candidate: () => calls.push('candidate'),
    }),
    /candidate verification passed, update the current anchor/u,
  );
  assert.deepEqual(calls, ['current', 'candidate']);
});

test('quarantined drift executes the current and reviewed candidate roots', async () => {
  // R2 decision table: no drift => current only; quarantined drift => current
  // then candidate; missing candidate verifier => fail before partial success.
  const stable = latestCanaryPlan('0.121.0', '0.121.0', '0.121.0');
  const quarantined = latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
    latestPublishedAt: '2026-08-27T20:35:25.000Z',
    minimumReleaseAgeMinutes: 1_440,
    now: Date.parse('2026-08-27T22:35:25.000Z'),
  });
  const calls = [];
  await executeLatestCanaryPlan(stable, {
    current: () => calls.push('stable'),
  });
  await executeLatestCanaryPlan(quarantined, {
    current: () => calls.push('current'),
    candidate: () => calls.push('candidate'),
  });
  assert.deepEqual(calls, ['stable', 'current', 'candidate']);
  await assert.rejects(
    executeLatestCanaryPlan(quarantined, { current() {} }),
    /requires every scheduled runtime verifier/u,
  );
});

test('candidate selection binds version pair, installed alias, and registry integrity', () => {
  // Supply-chain cause/effect graph: C1=registry drift, C2=one reviewed version
  // pair, C3=exact installed alias, C4=reviewed sha512 equals registry metadata.
  // Only C1+C2+C3+C4 permits later import; duplicates, missing fields, or a
  // same-version registry rewrite fail while all candidate code is still data.
  const plan = {
    oracle: '0.120.0', latest: '0.121.0', installed: '0.120.0', candidateRequired: true,
  };
  assert.equal(
    latestCandidateQualification(
      plan,
      [candidateQualification],
      'sha512-Y2FuZGlkYXRl',
      candidateDependencies,
    ),
    candidateQualification,
  );
  for (const qualifications of [
    [],
    [candidateQualification, candidateQualification],
    [{ ...candidateQualification, module: '' }],
    [{ ...candidateQualification, package_integrity: 'sha256-not-sha512' }],
  ]) {
    assert.throws(
      () => latestCandidateQualification(
        plan,
        qualifications,
        'sha512-Y2FuZGlkYXRl',
        candidateDependencies,
      ),
      /requires one exact qualification|module alias|sha512 integrity/u,
    );
  }
  assert.throws(
    () => latestCandidateQualification(
      plan,
      [candidateQualification],
      'sha512-dGFtcGVyZWQ=',
      candidateDependencies,
    ),
    /integrity does not match/u,
  );
  for (const dependencies of [
    undefined,
    {},
    { '@anthropic-ai/sdk-candidate': 'npm:@anthropic-ai/sdk@^0.121.0' },
    { '@anthropic-ai/sdk-candidate': 'npm:@anthropic-ai/sdk@0.122.0' },
  ]) {
    assert.throws(
      () => latestCandidateQualification(
        plan,
        [candidateQualification],
        'sha512-Y2FuZGlkYXRl',
        dependencies,
      ),
      /must use exact dependency/u,
    );
  }
  assert.equal(
    latestCandidateQualification(
      { oracle: '0.121.0', latest: '0.121.0', installed: '0.121.0' },
      undefined,
      undefined,
      undefined,
    ),
    undefined,
  );
});

test('candidate incompatibility is reported before a mature promotion warning', async () => {
  // Fault injection: the candidate runtime is the stronger fact. A failed
  // behavior proof must not be replaced by the administrative promotion error.
  const candidateFailure = new Error('candidate behavior mismatch');
  await assert.rejects(
    executeLatestCanaryPlan({ candidateRequired: true, promotionRequired: true }, {
      current() {},
      candidate() { throw candidateFailure; },
    }),
    (error) => error === candidateFailure,
  );
});

test('a stale installed SDK fails before it can impersonate the pin', () => {
  // R4: installed evidence must match before registry policy is considered.
  assert.throws(
    () => latestCanaryPlan('0.121.0', '0.121.0', '0.120.0'),
    /does not match oracle/u,
  );
});

test('ranges and malformed registry responses fail closed', () => {
  // R5: ambiguous version evidence is rejected before comparison.
  assert.throws(() => latestCanaryPlan('^0.121.0', '0.121.0', '0.121.0'), /must be exact/u);
  assert.throws(() => latestCanaryPlan('0.121.0', 'latest', '0.121.0'), /invalid/u);
  assert.throws(() => latestCanaryPlan('0.121.0', ['0.121.0'], '0.121.0'), /invalid/u);
});

test('registry drift without one valid release-age policy fails closed', () => {
  // R5: drift cannot invent a grace period when any policy coordinate is absent.
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.121.0', '0.120.0'),
    /valid minimum-release-age policy/u,
  );
  assert.throws(
    () => latestCanaryPlan('0.120.0', '0.121.0', '0.120.0', {
      latestPublishedAt: 'invalid', minimumReleaseAgeMinutes: 1_440, now: Date.now(),
    }),
    /valid minimum-release-age policy/u,
  );
});

test('the release canary reaches both official TypeScript and Python SDK oracles', () => {
  // Orchestration cause/effect graph: C1=the release entry executes the local
  // multi-language oracle check; C2=it executes the online Python wheel canary;
  // C3=it executes the TypeScript runtime canary; C4=the reviewed candidate
  // executes the same cross-version behavior suites as stable anchors. Effect E1=no language can be
  // silently dropped while the outer `test:sdk-latest-canary` command remains
  // green. Decision table: C1+C2+C3=>E1; removal of any exact edge=>reject.
  const source = readFileSync(new URL('./sdk_latest_canary.mjs', import.meta.url), 'utf8');
  for (const edge of [
    "'check'",
    "'check:python:online'",
    "'sdk_latest_runtime_canary.mjs'",
    "'test:managed-sdk-candidate-matrix'",
  ]) {
    assert.ok(source.includes(edge), `release canary is missing ${edge}`);
  }
  assert.match(
    source,
    /verifyRuntime\(candidate\);\s*verifyCandidateMatrix\(\s*candidateQualification\.module,\s*candidateQualification\.candidate_version,/u,
    'the exact candidate passes delta runtime proof before the shared behavior matrix',
  );
  const scripts = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')).scripts;
  assert.match(scripts['test:sdk-latest-canary'], /npm run test:sdk-python-runtime/u);
  assert.equal(
    scripts['test:sdk-python-runtime'],
    'node conformance/managed_python_sdk_runtime_e2e.mjs',
  );
  const pythonRunner = readFileSync(
    new URL('./managed_python_sdk_runtime_e2e.mjs', import.meta.url),
    'utf8',
  );
  // Cross-language authority decision table:
  // TS current == Python shape -> generate both Python contract corpora from
  // the current TS oracle; TS candidate adds a field first -> candidate stays
  // on the TS-only qualification axis and cannot widen Python witnesses.
  // Effect: the Python gate changes only when its own reviewed oracle advances.
  assert.match(
    pythonRunner,
    /upstream-oracle\.generated\.json'[\s\S]*\.current\.module/u,
    'Python contract generation is pinned to the current TS oracle, not the registry candidate',
  );
  assert.doesNotMatch(
    pythonRunner,
    /const module = '@anthropic-ai\/sdk-candidate'/u,
    'a quarantined TS candidate cannot become the Python contract authority',
  );
  assert.match(
    pythonRunner,
    /const responseContracts = writePythonResponseContracts\(temporary\);[\s\S]*writePythonRequestContracts\(temporary\)[\s\S]*exercisePythonRequestContracts\(python, requestContracts\)[\s\S]*exercisePythonResponseContracts\(python, responseContracts\)[\s\S]*await withScenarioServer/u,
    'the declaration-derived request and response proofs precede selected real-process lifecycles',
  );
  assert.match(
    pythonRunner,
    /exerciseHistoricalMatrix\([\s\S]*responseContracts[\s\S]*AWAKEN_MANAGED_PYTHON_RESPONSE_CONTRACTS: responseContracts/u,
    'the same current response corpus reaches every historical wheel',
  );
  const responseDriver = readFileSync(
    new URL('./managed_python_sdk_response_contract_e2e.py', import.meta.url),
    'utf8',
  );
  for (const edge of [
    'PYTHON_DECLARATION_VARIANCES',
    'TypeAdapter(response_type).json_schema()',
    'assert_witness_coverage(contract["schema"])',
    'assert decoded == witness',
    'probe.warning_modes == ["error", False]',
    'exercise_sync(anthropic, httpx2, operations, contracts)',
    'exercise_async(anthropic, httpx2, operations, contracts)',
  ]) {
    assert.ok(responseDriver.includes(edge), `Python response proof is missing ${edge}`);
  }
  const requestDriver = readFileSync(
    new URL('./managed_python_sdk_request_contract_e2e.py', import.meta.url),
    'utf8',
  );
  for (const edge of [
    'KNOWN_NULL_QUERY_VARIANCES',
    'observed_variances == KNOWN_NULL_QUERY_VARIANCES',
    'actual == expected_without_empty_field',
    'exercise_declared_request_witnesses(',
    'exercise_sync(bundle)',
    'exercise_async(bundle)',
  ]) {
    assert.ok(requestDriver.includes(edge), `Python request proof is missing ${edge}`);
  }
  const historicalDriver = readFileSync(
    new URL('./managed_python_sdk_matrix_e2e.py', import.meta.url),
    'utf8',
  );
  for (const edge of [
    'exercise_declared_request_witnesses(',
    'exercise_pathlike_upload_change_point(',
    'exercise_current_response_compatibility(',
    'verify_declarations=False',
    'current response corpus lacks historical operations',
    'assert sync_count == async_count',
  ]) {
    assert.ok(historicalDriver.includes(edge), `historical response proof is missing ${edge}`);
  }
  const historicalRequestDriver = readFileSync(
    new URL('./managed_python_sdk_request_contract.py', import.meta.url),
    'utf8',
  );
  for (const edge of [
    'UNICODE_WORKER_HEADER_REJECTING_VERSIONS',
    'MULTIPART_HEADER_NAME_REJECTING_VERSIONS',
    'exact empty-path rejection closure',
    'exact Unicode worker-header change point',
    'exact multipart-header change point',
  ]) {
    assert.ok(
      historicalRequestDriver.includes(edge),
      `historical request proof is missing ${edge}`,
    );
  }
});
