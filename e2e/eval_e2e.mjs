// e2e for the fixture-driven eval harness (#4): drive the real `awaken-eval`
// binary over a dataset file and assert it replays each case through the real
// runtime and scores it, exiting non-zero when an expectation fails.
//
// Run: (from e2e/)  node eval_e2e.mjs

import { spawnSync } from 'node:child_process';
import { writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

function runEval(dataset) {
  const dir = mkdtempSync(path.join(tmpdir(), 'awaken-eval-'));
  const file = path.join(dir, 'dataset.json');
  writeFileSync(file, JSON.stringify(dataset));
  const res = spawnSync(
    'cargo',
    ['run', '--quiet', '-p', 'awaken-eval', '--bin', 'awaken-eval', '--', file],
    { cwd: REPO_ROOT, encoding: 'utf8', env: process.env, maxBuffer: 16 * 1024 * 1024 },
  );
  if (res.error) throw res.error;
  return { status: res.status, report: res.stdout ? JSON.parse(res.stdout) : null, stderr: res.stderr };
}

// A dataset every case of which passes: the replayed output contains "42" and
// the run ends naturally.
const passing = {
  name: 'e2e-pass',
  cases: [
    {
      id: 'answers-42',
      instructions: 'be terse',
      input: 'what is the answer?',
      script: [{ text: 'the answer is 42' }],
      expectations: [
        { kind: 'output_contains', substring: '42' },
        { kind: 'succeeded' },
      ],
    },
    {
      // A tool-scripted case: turn 1 calls `search`, turn 2 answers. The eval
      // registers an echo tool for `search`, so the call runs on the real engine.
      id: 'uses-a-tool',
      input: 'find it',
      script: [
        { tool_calls: [{ tool_id: 'search', arguments: { q: 'answer' } }] },
        { text: 'found: 42' },
      ],
      expectations: [
        { kind: 'tool_called', tool_id: 'search' },
        { kind: 'output_contains', substring: '42' },
      ],
    },
  ],
};

// A dataset with a failing expectation (output does not contain "999").
const failing = {
  name: 'e2e-fail',
  cases: [
    {
      id: 'wrong',
      input: 'hello',
      script: [{ text: 'hi there' }],
      expectations: [{ kind: 'output_contains', substring: '999' }],
    },
  ],
};

// 1) A fully-passing dataset exits 0 and reports every case passed.
{
  const { status, report } = runEval(passing);
  assert.equal(status, 0, 'a fully-passing dataset exits 0');
  assert.equal(report.dataset, 'e2e-pass');
  assert.equal(report.scores.length, 2);
  assert.ok(
    report.scores.every((s) => s.results.every((r) => r.passed)),
    'every expectation (including a tool call) passed by replaying through the real runtime',
  );
  console.log('  ok: passing dataset (text + tool-scripted) replays and scores 0');
}

// 2) A dataset with a failing expectation exits non-zero and marks it failed.
{
  const { status, report } = runEval(failing);
  assert.notEqual(status, 0, 'a failing expectation exits non-zero');
  assert.equal(report.scores[0].results[0].passed, false);
  assert.ok(report.scores[0].results[0].detail.includes('999'));
  console.log('  ok: failing dataset scores and gates via exit code');
}

console.log('E2E PASS: awaken-eval replays datasets through the real runtime and gates on scores.');
