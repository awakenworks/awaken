import assert from 'node:assert/strict';
import { test } from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import * as reports from './llm_eval_metrics.mjs';
import {
  aggregateUsage,
  evaluateThresholds,
  makeEvaluation,
  percentile,
  taggedFactMetrics,
  usageWithFallback,
} from './llm_eval_metrics.mjs';

test('scores recall, precision, stale facts and unknown tagged contamination', () => {
  const scored = taggedFactMetrics({
    expected: ['AWKFACT_A', 'AWKFACT_B'],
    forbidden: ['AWKFACT_STALE'],
    text: 'AWKFACT_A AWKFACT_STALE AWKFACT_UNKNOWN',
  });
  assert.equal(scored.recall, 0.5);
  assert.equal(scored.precision, 1 / 3);
  assert.equal(scored.f1, 0.4);
  assert.equal(scored.stale_fact_rate, 1);
  assert.equal(scored.contamination_rate, 2 / 3);
  assert.deepEqual(scored.missing, ['AWKFACT_B']);
});

test('deduplicates repeated facts so verbose output cannot inflate quality', () => {
  const scored = taggedFactMetrics({
    expected: ['AWKFACT_A', 'AWKFACT_B'],
    text: 'AWKFACT_A AWKFACT_A AWKFACT_A',
  });
  assert.equal(scored.recall, 0.5);
  assert.equal(scored.precision, 1);
  assert.equal(scored.f1, 2 / 3);
});

test('threshold failures and malformed rules fail closed', () => {
  assert.deepEqual(
    evaluateThresholds({ recall: 0.7, latency_ms: 20 }, {
      recall: { operator: 'gte', value: 0.8 },
      latency_ms: { operator: 'lte', value: 50 },
    }),
    [{ metric: 'recall', actual: 0.7, operator: 'gte', value: 0.8 }],
  );
  assert.throws(() => evaluateThresholds({}, { recall: { operator: 'gte', value: 1 } }), /missing metric/);
  assert.throws(
    () => evaluateThresholds({ recall: 1 }, { recall: { operator: 'equal', value: 1 } }),
    /unsupported threshold operator/,
  );
});

test('aggregates only paired model-request usage and exposes missing telemetry', () => {
  assert.deepEqual(aggregateUsage([{ type: 'agent.message' }]), {
    observed: false, input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0,
  });
  assert.deepEqual(aggregateUsage([
    { type: 'span.model_request_end', model_usage: { input_tokens: 2, output_tokens: 3 } },
    { type: 'span.model_request_end', model_usage: { input_tokens: 5, cache_read_input_tokens: 7 } },
  ]), { observed: true, input_tokens: 7, output_tokens: 3, cache_read_input_tokens: 7 });
});

test('usage fallback is deterministic across event, summary, absent and malformed inputs', () => {
  const event = {
    observed: true, input_tokens: 2, output_tokens: 3, cache_read_input_tokens: 4,
  };
  assert.equal(usageWithFallback(event, { input_tokens: 99 }), event, 'events remain authoritative');
  assert.deepEqual(usageWithFallback(aggregateUsage([]), {
    input_tokens: 5, output_tokens: 7, cache_read_input_tokens: 11,
    cache_creation_input_tokens: 13,
  }), {
    observed: true, input_tokens: 5, output_tokens: 7,
    cache_read_input_tokens: 11, cache_creation_input_tokens: 13,
  });
  assert.deepEqual(usageWithFallback(aggregateUsage([]), undefined), {
    observed: false, input_tokens: 0, output_tokens: 0,
    cache_read_input_tokens: 0, cache_creation_input_tokens: 0,
  });
  assert.deepEqual(usageWithFallback(aggregateUsage([]), {
    input_tokens: -1, output_tokens: 'not-a-number',
  }), {
    observed: true, input_tokens: 0, output_tokens: 0,
    cache_read_input_tokens: 0, cache_creation_input_tokens: 0,
  });
});

test('nearest-rank percentile preserves boundaries and rejects empty samples', () => {
  assert.equal(percentile([40, 10, 30, 20], 0), 10);
  assert.equal(percentile([40, 10, 30, 20], 0.5), 20);
  assert.equal(percentile([40, 10, 30, 20], 0.95), 40);
  assert.throws(() => percentile([], 0.5), /at least one sample/);
  assert.throws(() => percentile([1], 2), /\[0,1\]/);
});

test('evaluation document is versioned and carries threshold violations', () => {
  const evaluation = makeEvaluation({
    suite: 'fixture', subject: 'memory', backend: 'native', model: 'fake', sampleSize: 1,
    metrics: { recall: 0 }, thresholds: { recall: { operator: 'gte', value: 1 } },
  });
  assert.equal(evaluation.schema_version, 1);
  assert.equal(evaluation.pass, false);
  assert.equal(evaluation.violations[0].metric, 'recall');
});


// Report IO cause/effect table: I1 evaluation or compatibility schema -> one
// existing stdout+artifact sink preserves exact JSON/prefix/path; I2 no output
// dir -> stdout only; I3 write error -> throw (never emit a qualified result).
// Metric thresholds/acceptance remain in emitEvaluation after that same sink.
test('public report uses the original artifact selector and exact document once', t => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'provider-report-law-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const lines = []; t.mock.method(console, 'log', line => lines.push(line));
  const document = { version: 1, status: 'failed', results: [{ id: 'fixture', status: 'failed', attempts: [] }] };
  reports.emitPublicReport('AWAKEN_PROVIDER_COMPAT', 'provider-compat', document, { AWAKEN_EVAL_ARTIFACT_DIR: directory });
  assert.deepEqual(lines, [`AWAKEN_PROVIDER_COMPAT ${JSON.stringify(document)}`]);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(directory, 'provider-compat.json'), 'utf8')), document);
  assert.deepEqual(fs.readdirSync(directory), ['provider-compat.json'], 'I1 no parallel ledger');
});

test('evaluation retains its original prefix thresholds and artifact IO failure', t => {
  const lines = []; t.mock.method(console, 'log', line => lines.push(line));
  const good = makeEvaluation({ suite: 'fixture', subject: 'fixture', backend: 'fixture', model: 'fixture',
    sampleSize: 1, metrics: { recall: 1 }, thresholds: { recall: { operator: 'gte', value: 1 } } });
  reports.emitEvaluation(good, {});
  assert.equal(lines[0], `AWAKEN_LLM_EVAL ${JSON.stringify(good)}`);
  const bad = { ...good, pass: false, violations: [{ actual: 0 }] };
  assert.throws(() => reports.emitEvaluation(bad, {}), /thresholds failed/u);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'report-io-refusal-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const file = path.join(root, 'not-a-directory'); fs.writeFileSync(file, 'owned');
  assert.throws(() => reports.emitPublicReport('AWAKEN_PROVIDER_COMPAT', 'provider-compat', {},
    { AWAKEN_EVAL_ARTIFACT_DIR: file }));
});
