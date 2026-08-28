import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import { managedSdkOwnerProcessEnvironment } from './managed_sdk_process_environment.mjs';

const hook = path.resolve(import.meta.dirname, 'managed_sdk_package_hook.mjs');
const behaviorRunner = path.resolve(import.meta.dirname, 'run_managed_sdk_behavior_owners.mjs');

function environmentWithoutCandidateSelection() {
  const environment = { ...process.env };
  delete environment.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE;
  delete environment.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION;
  delete environment.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
  return environment;
}

test('behavior-owner prebuild uses one explicit Cargo authority', () => {
  // Build-authority graph: an existing CI/user cache remains authoritative and
  // is consumed before behavior timeouts begin; a suite-specific override wins
  // when isolation capacity exists. With neither, the worktree target is the
  // deterministic fallback. Caller state remains immutable in every partition.
  const inherited = {
    CARGO_TARGET_DIR: '/global/shared-target',
    PATH: '/bin',
  };
  const isolated = managedSdkOwnerProcessEnvironment(inherited, '/worktree/e2e');
  assert.equal(isolated.CARGO_TARGET_DIR, '/global/shared-target');
  assert.equal(isolated.PATH, '/bin');
  assert.equal(inherited.CARGO_TARGET_DIR, '/global/shared-target');

  const configured = managedSdkOwnerProcessEnvironment({
    ...inherited,
    AWAKEN_MANAGED_SDK_CARGO_TARGET_DIR: '/suite/target',
  }, '/worktree/e2e');
  assert.equal(configured.CARGO_TARGET_DIR, '/suite/target');
  assert.equal(
    managedSdkOwnerProcessEnvironment({ PATH: '/bin' }, '/worktree/e2e').CARGO_TARGET_DIR,
    '/worktree/target',
  );
});

function executeSelection(moduleName, expectedVersion = undefined) {
  const sdk = resolveSdkPackage(moduleName);
  const directory = mkdtempSync(path.resolve(tmpdir(), 'awaken-managed-sdk-selection-'));
  const resolutionFile = path.resolve(directory, 'resolution.jsonl');
  const result = spawnSync(process.execPath, [
    '--import', hook,
    '--input-type=module',
    '--eval',
    "await import('@anthropic-ai/sdk');",
  ], {
    encoding: 'utf8',
    env: {
      ...process.env,
      AWAKEN_MANAGED_SDK_PACKAGE_ROOT: sdk.root,
      AWAKEN_MANAGED_SDK_PACKAGE_VERSION: expectedVersion ?? sdk.version,
      AWAKEN_MANAGED_SDK_RESOLUTION_FILE: resolutionFile,
    },
  });
  return { directory, resolutionFile, result, sdk };
}

test('canonical imports resolve inside every selected exact SDK package', () => {
  // Differential causal graph: C1 select the current root; C2 select the
  // reviewed candidate root; C3 application source imports the same canonical
  // package name. E1 each process evaluates a file inside only its selected
  // root and records the exact manifest version. This proves candidate replay
  // does not require source edits, copied tests, or mutable node_modules state.
  for (const moduleName of ['@anthropic-ai/sdk-current', '@anthropic-ai/sdk-candidate']) {
    const execution = executeSelection(moduleName);
    try {
      assert.equal(execution.result.status, 0, execution.result.stderr);
      const records = readFileSync(execution.resolutionFile, 'utf8')
        .trim()
        .split('\n')
        .map((line) => JSON.parse(line));
      assert.ok(records.length > 0);
      assert.ok(records.every(({ version }) => version === execution.sdk.version));
      assert.ok(records.every(({ url }) => {
        const relative = path.relative(execution.sdk.root, realpathSync(new URL(url)));
        return !relative.startsWith('..') && !path.isAbsolute(relative);
      }));
    } finally {
      rmSync(execution.directory, { recursive: true, force: true });
    }
  }
});

test('package selection rejects an adjacent expected version before application import', () => {
  // Fault injection: preserve the reviewed candidate bytes/root while claiming
  // the baseline version. The preload must terminate before canonical package
  // resolution, closing the false-evidence path at the process boundary.
  const execution = executeSelection('@anthropic-ai/sdk-candidate', '0.121.0');
  try {
    assert.notEqual(execution.result.status, 0);
    assert.match(execution.result.stderr, /does not match expected/u);
  } finally {
    rmSync(execution.directory, { recursive: true, force: true });
  }
});

test('candidate behavior replay cannot opt into the historical subset rule', () => {
  // Admission partition: exact historical anchors may project a monotonic
  // operation subset; a reviewed candidate must retain the complete current
  // identity set. Even a correctly installed candidate root/version therefore
  // fails before owner execution when historical mode is requested.
  const sdk = resolveSdkPackage('@anthropic-ai/sdk-candidate');
  const result = spawnSync(process.execPath, [behaviorRunner], {
    encoding: 'utf8',
    env: {
      ...process.env,
      ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: sdk.root,
      ANTHROPIC_SDK_CONFORMANCE_CANDIDATE: '@anthropic-ai/sdk-candidate',
      ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION: sdk.version,
      AWAKEN_MANAGED_SDK_HISTORICAL_SUBSET: '1',
    },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /candidate SDK cannot use the historical operation-subset rule/u);
});

test('candidate behavior replay requires the exact alias before any owner starts', () => {
  // Admission causal graph: candidate version + exact root without the reviewed
  // alias would let generic owners use the selected package hook while helper
  // owners silently omit the candidate from their official-client matrix.
  // The runner must reject that split-brain configuration before extraction,
  // compilation, or server startup; the full 0.122 replay exposed this edge.
  const sdk = resolveSdkPackage('@anthropic-ai/sdk-candidate');
  const result = spawnSync(process.execPath, [behaviorRunner], {
    encoding: 'utf8',
    env: {
      ...environmentWithoutCandidateSelection(),
      ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: sdk.root,
      ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION: sdk.version,
    },
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /require the reviewed exact package alias/u);
  assert.doesNotMatch(result.stdout, /\[managed-sdk 1\//u);
});

test('version-projected capability inputs form one closed boolean domain', () => {
  // Configuration fault partition: the owner runner emits only `0` or `1` for
  // SDK-derived GA availability. A typo or ad-hoc boolean spelling must fail at
  // module admission before a server starts; otherwise a historical SDK could
  // silently execute a different owner branch and still appear compatible.
  for (const [script, variable, pattern] of [
    ['management_files_models_e2e.mjs', 'AWAKEN_MANAGED_SDK_HAS_GA_FILES', /GA Files capability/u],
    ['management_skills_e2e.mjs', 'AWAKEN_MANAGED_SDK_HAS_GA_SKILLS', /GA Skills capability/u],
    ['managed_dream_e2e.ts', 'AWAKEN_MANAGED_SDK_HAS_DREAMS', /Dreams capability/u],
  ]) {
    const result = spawnSync(process.execPath, [path.resolve(import.meta.dirname, '..', script)], {
      encoding: 'utf8',
      env: { ...process.env, [variable]: 'false' },
    });
    assert.notEqual(result.status, 0, script);
    assert.match(result.stderr, pattern, script);
  }
});
