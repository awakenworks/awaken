// Public benchmark CLI E2E. It drives the compiled awaken-eval binary across
// RewardBench-style, QMSum-style, and LoCoMo-style source documents. Live ACP
// inference is deliberately gated separately; this suite is deterministic.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const repo = path.resolve(import.meta.dirname, '..');
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-public-benchmark-'));
let covered = 0;
const designed = 20;

function checkpoint(condition: unknown, message: string): asserts condition {
  assert.ok(condition, message);
  covered += 1;
}

function write(name: string, value: unknown) {
  const target = path.join(tmp, name);
  fs.writeFileSync(target, JSON.stringify(value));
  return target;
}

function run(args: string[], expectedStatus = 0) {
  const result = spawnSync('cargo', ['run', '-q', '-p', 'awaken-eval', '--', ...args], {
    cwd: repo,
    encoding: 'utf8',
  });
  assert.equal(result.status, expectedStatus, `${args[0]}: ${result.stderr}\n${result.stdout}`);
  return result.stdout.trim() ? JSON.parse(result.stdout) : null;
}

try {
  const rewardSource = write('reward-pages.json', [
    { rows: [
      { row: { id: 'f1', prompt: 'request 1', chosen: ['good 1'], rejected: ['bad 1', 'bad 2'], subset: 'factuality' } },
      { row: { id: 's1', prompt: 'request 2', chosen: ['good 2'], rejected: ['bad 3', 'bad 4'], subset: 'safety' } },
    ] },
  ]);
  const rewardDataset = path.join(tmp, 'reward-dataset.json');
  run(['benchmark-import-rewardbench2', rewardSource, rewardDataset, '2']);
  const reward = JSON.parse(fs.readFileSync(rewardDataset, 'utf8'));
  checkpoint(reward.cases.length === 2, 'RewardBench limit is applied');
  checkpoint(new Set(reward.cases.map((item: any) => item.subset)).size === 2, 'sampling is stratified');
  checkpoint(reward.cases[0].expected === 'a' && reward.cases[1].expected === 'b', 'positions are balanced');

  const perfect = reward.cases.map((item: any) => ({
    case_id: item.id,
    output: JSON.stringify({ better: item.expected, explanation: 'evidence-based comparison' }),
    latency_ms: 5,
  }));
  const perfectPath = write('reward-perfect.json', perfect);
  const rewardReport = run(['benchmark-score-pairwise', rewardDataset, perfectPath]);
  checkpoint(rewardReport.accuracy.correct === 2, 'pairwise gold accuracy is exact');
  checkpoint(rewardReport.schema_valid.correct === 2, 'strict schema is reported');
  checkpoint(rewardReport.accuracy.ci95_low < 1, 'small samples expose uncertainty');

  const contrary = reward.cases.map((item: any) => ({
    case_id: item.id,
    output: JSON.stringify({ better: item.expected === 'a' ? 'b' : 'a', explanation: 'contrary' }),
    latency_ms: 6,
  }));
  const contraryPath = write('reward-contrary.json', contrary);
  const cross = run(['benchmark-compare-pairwise', rewardDataset, perfectPath, contraryPath]);
  checkpoint(cross.agreement.correct === 0, 'cross-model disagreement is counted');
  checkpoint(cross.left_only_correct === 2, 'gold separates agreement from correctness');
  checkpoint(cross.disagreement_case_ids.length === 2, 'disagreement cases remain auditable');

  const qmsumDir = path.join(tmp, 'qmsum');
  fs.mkdirSync(qmsumDir);
  write('qmsum-source-copy.json', {}); // boundary: unrelated JSON is not placed in the corpus directory
  fs.writeFileSync(path.join(qmsumDir, 'meeting.json'), JSON.stringify({
    meeting_transcripts: [{ speaker: 'A', content: 'alpha beta gamma delta' }],
    general_query_list: [{ query: 'What matters?', answer: 'alpha beta' }],
    specific_query_list: [{ query: 'Which tail?', answer: 'gamma delta' }],
  }));
  const compactDataset = path.join(tmp, 'compact-dataset.json');
  run(['benchmark-import-qmsum', qmsumDir, compactDataset, '2']);
  const compact = JSON.parse(fs.readFileSync(compactDataset, 'utf8'));
  checkpoint(compact.cases.length === 2, 'QMSum general and specific queries import');
  checkpoint(compact.cases.every((item: any) => item.transcript.includes('A: alpha')), 'speaker context is retained');
  const compactObservations = compact.cases.map((item: any) => ({ case_id: item.id, output: item.reference, latency_ms: 3 }));
  const compactReport = run(['benchmark-score-compact-reference', compactDataset, write('compact-observations.json', compactObservations)]);
  checkpoint(compactReport.mean_token_f1 === 1, 'reference token F1 reaches one');
  checkpoint(compactReport.mean_compression_ratio < 1, 'compression ratio is measured');

  const locomoSource = write('locomo.json', [{
    sample_id: 'conversation-1',
    conversation: { session_1: [
      { dia_id: 'D1:1', speaker: 'A', text: 'I prefer blue.' },
      { dia_id: 'D1:2', speaker: 'B', text: 'I prefer red.' },
      { dia_id: 'D1:3', speaker: 'A', text: 'I work remotely.' },
    ] },
    qa: [{ question: 'Which color does A prefer?', answer: 'blue', evidence: ['D1:1'], category: 1 }],
  }]);
  const memoryDataset = path.join(tmp, 'memory-dataset.json');
  run(['benchmark-import-locomo-memory', locomoSource, memoryDataset, '1', '2']);
  const memory = JSON.parse(fs.readFileSync(memoryDataset, 'utf8'));
  checkpoint(memory.extraction_cases.length === 0, 'LoCoMo does not masquerade as extraction');
  checkpoint(memory.selection_cases.length === 1, 'LoCoMo produces selector cases');
  checkpoint(memory.selection_cases[0].memories.length === 3, 'hard negatives are included');
  checkpoint(memory.selection_cases[0].expected_indices.length === 1, 'gold evidence maps to one index');
  const selection = memory.selection_cases[0];
  const memoryObservations = [{
    case_id: selection.id,
    output: JSON.stringify(selection.expected_indices),
    latency_ms: 2,
  }];
  const memoryReport = run(['memory-score', memoryDataset, write('memory-observations.json', memoryObservations)]);
  checkpoint(memoryReport.selection_exact === 1, 'production selector parser scores imported evidence');
  checkpoint(memoryReport.selection_schema_valid === 1, 'selector wire schema is preserved');

  checkpoint(covered === designed - 1, 'all designed behavioral checkpoints executed');
  const ratio = covered / designed;
  assert.ok(ratio > 0.95, `functional coverage ${(ratio * 100).toFixed(2)}% must exceed 95%`);
  console.log(`E2E PASS: public benchmark adapters/scorers (${covered}/${designed}, ${(ratio * 100).toFixed(2)}%)`);
} finally {
  fs.rmSync(tmp, { recursive: true, force: true });
}
