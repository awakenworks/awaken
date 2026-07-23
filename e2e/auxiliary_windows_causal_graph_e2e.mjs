// Causal-graph system suite for Transcript windows + Memory/Compact/Outcome.
// Each case covers a minimal cause combination and asserts externally observable
// effects. Internal branch contracts remain in their owning Rust package tests.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.dirname(fileURLToPath(import.meta.url));
const LIVE = process.env.E2E_CAUSAL_LIVE === '1';

const REQUIRED_CAUSES = new Set([
  'terminal-ended',
  'duplicate-restart',
  'current-run-input',
  'soft-hard-compact',
  'outcome-evaluation',
  'native-acp-a2a',
  'shared-memory-store',
]);
const REQUIRED_EFFECTS = new Set([
  'frozen-window',
  'exactly-once-memory',
  'bounded-recall',
  'nonblocking-or-joined-compact',
  'no-history-loss',
  'guarded-outcome-transition',
  'runtime-neutral-lifecycle',
  'cross-runtime-recall',
]);
const DETERMINISTIC_CAUSES = new Set(
  [...REQUIRED_CAUSES].filter((cause) => cause !== 'shared-memory-store'),
);
const DETERMINISTIC_EFFECTS = new Set(
  [...REQUIRED_EFFECTS].filter((effect) => effect !== 'cross-runtime-recall'),
);

const cases = [
  {
    id: 'CG-M1',
    file: 'managed_memory_e2e.mjs',
    causes: ['terminal-ended', 'current-run-input'],
    effects: ['frozen-window', 'bounded-recall'],
  },
  {
    id: 'CG-M2',
    file: 'managed_memory_extraction_stage_recovery_e2e.mjs',
    causes: ['terminal-ended', 'duplicate-restart'],
    effects: ['frozen-window', 'exactly-once-memory'],
  },
  {
    id: 'CG-C1',
    file: 'managed_compaction_e2e.mjs',
    causes: ['soft-hard-compact'],
    effects: ['nonblocking-or-joined-compact', 'no-history-loss'],
  },
  {
    id: 'CG-C2',
    file: 'managed_compaction_durable_e2e.mjs',
    causes: ['soft-hard-compact', 'duplicate-restart'],
    effects: ['nonblocking-or-joined-compact', 'no-history-loss'],
  },
  {
    id: 'CG-G1',
    file: 'managed_outcome_runtime_matrix_e2e.ts',
    causes: ['outcome-evaluation', 'native-acp-a2a'],
    effects: ['guarded-outcome-transition', 'runtime-neutral-lifecycle'],
  },
  {
    id: 'CG-A1',
    file: 'cross_protocol_a2a_continuity_e2e.mjs',
    causes: ['native-acp-a2a'],
    effects: ['runtime-neutral-lifecycle'],
  },
  {
    id: 'CG-R1',
    file: 'acp_runtime_memory_matrix_e2e.mjs',
    live: true,
    causes: ['native-acp-a2a', 'shared-memory-store'],
    effects: ['runtime-neutral-lifecycle', 'cross-runtime-recall'],
  },
];

function union(field, selected) {
  return new Set(selected.flatMap((testCase) => testCase[field]));
}

function assertCovered(required, actual, label) {
  const missing = [...required].filter((item) => !actual.has(item));
  assert.deepEqual(missing, [], `causal graph has uncovered ${label}: ${missing.join(', ')}`);
}

const selected = cases.filter((testCase) => LIVE || !testCase.live);
const requiredCauses = LIVE ? REQUIRED_CAUSES : DETERMINISTIC_CAUSES;
const requiredEffects = LIVE ? REQUIRED_EFFECTS : DETERMINISTIC_EFFECTS;
assertCovered(requiredCauses, union('causes', selected), 'causes');
assertCovered(requiredEffects, union('effects', selected), 'effects');

for (const testCase of selected) {
  console.log(
    `\n[${testCase.id}] causes=${testCase.causes.join('+')} -> effects=${testCase.effects.join('+')}`,
  );
  const result = spawnSync(process.execPath, [path.join(ROOT, testCase.file)], {
    cwd: ROOT,
    env: process.env,
    stdio: 'inherit',
  });
  assert.equal(
    result.status,
    0,
    `${testCase.id} failed (${testCase.file}, signal=${result.signal ?? 'none'})`,
  );
}

console.log(
  `E2E PASS: auxiliary causal graph covered ${requiredCauses.size} causes and `
    + `${requiredEffects.size} effects${LIVE ? ' including real ACP runtimes' : ''}.`,
);
