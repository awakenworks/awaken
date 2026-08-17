// Shared, machine-readable scoring for stochastic Managed Agent evaluations.
// A real LLM produces output; exact randomized fact tokens make scoring
// reproducible without turning a second subjective model into the oracle.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

const OPERATORS = new Set(['gte', 'lte']);

export function taggedFactMetrics({ expected, forbidden = [], text }) {
  const expectedSet = new Set(expected);
  const forbiddenSet = new Set(forbidden);
  const matched = [...expectedSet].filter((fact) => text.includes(fact));
  const forbiddenMatched = [...forbiddenSet].filter((fact) => text.includes(fact));
  const observed = new Set(
    [...text.matchAll(/AWKFACT_[A-Z0-9_]+/gu)].map((match) => match[0]),
  );
  const unknown = [...observed].filter(
    (fact) => !expectedSet.has(fact) && !forbiddenSet.has(fact),
  );
  const truePositive = matched.length;
  const falsePositive = unknown.length + forbiddenMatched.length;
  const recall = expectedSet.size === 0 ? 1 : truePositive / expectedSet.size;
  const precision = truePositive + falsePositive === 0
    ? (expectedSet.size === 0 ? 1 : 0)
    : truePositive / (truePositive + falsePositive);
  const f1 = precision + recall === 0 ? 0 : (2 * precision * recall) / (precision + recall);
  return {
    precision,
    recall,
    f1,
    contamination_rate: observed.size === 0 ? 0 : falsePositive / observed.size,
    stale_fact_rate: forbiddenSet.size === 0 ? 0 : forbiddenMatched.length / forbiddenSet.size,
    matched,
    missing: [...expectedSet].filter((fact) => !text.includes(fact)),
    forbidden_matched: forbiddenMatched,
    unknown,
  };
}

export function percentile(samples, quantile) {
  assert.ok(Number.isFinite(quantile) && quantile >= 0 && quantile <= 1, 'quantile must be in [0,1]');
  assert.ok(samples.length > 0, 'percentile requires at least one sample');
  const sorted = [...samples].sort((left, right) => left - right);
  return sorted[Math.ceil(quantile * sorted.length) - 1] ?? sorted[0];
}

export function aggregateUsage(events) {
  const usage = { input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0 };
  let observed = false;
  for (const event of events) {
    if (event.type !== 'span.model_request_end' || !event.model_usage) continue;
    observed = true;
    usage.input_tokens += Number(event.model_usage.input_tokens ?? 0);
    usage.output_tokens += Number(event.model_usage.output_tokens ?? 0);
    usage.cache_read_input_tokens += Number(event.model_usage.cache_read_input_tokens ?? 0);
  }
  return { observed, ...usage };
}

/// Prefer per-request event telemetry, but accept a resource-level cumulative
/// usage projection when an API intentionally hides the auxiliary event spans.
/// Provider omissions and malformed counters become explicit zeroes; they never
/// produce NaN metrics or accidentally double-count both sources.
export function usageWithFallback(eventUsage, summary) {
  if (eventUsage.observed) return eventUsage;
  const fields = [
    'input_tokens',
    'output_tokens',
    'cache_read_input_tokens',
    'cache_creation_input_tokens',
  ];
  const hasSummary = summary && fields.some((field) => Object.hasOwn(summary, field));
  const value = (field) => {
    const number = Number(summary?.[field] ?? 0);
    return Number.isFinite(number) && number >= 0 ? number : 0;
  };
  return {
    observed: Boolean(hasSummary),
    input_tokens: value('input_tokens'),
    output_tokens: value('output_tokens'),
    cache_read_input_tokens: value('cache_read_input_tokens'),
    cache_creation_input_tokens: value('cache_creation_input_tokens'),
  };
}

export function evaluateThresholds(metrics, thresholds) {
  const violations = [];
  for (const [name, rule] of Object.entries(thresholds)) {
    assert.ok(name in metrics, `threshold refers to missing metric ${name}`);
    assert.ok(OPERATORS.has(rule.operator), `unsupported threshold operator ${rule.operator}`);
    assert.ok(Number.isFinite(rule.value), `threshold ${name} must be finite`);
    assert.ok(Number.isFinite(metrics[name]), `metric ${name} must be finite`);
    const passed = rule.operator === 'gte'
      ? metrics[name] >= rule.value
      : metrics[name] <= rule.value;
    if (!passed) violations.push({ metric: name, actual: metrics[name], ...rule });
  }
  return violations;
}

export function makeEvaluation({ suite, subject, backend, model, sampleSize, metrics, thresholds, details = {} }) {
  const violations = evaluateThresholds(metrics, thresholds);
  return {
    schema_version: 1,
    suite,
    subject,
    backend,
    model,
    sample_size: sampleSize,
    measured_at: new Date().toISOString(),
    metrics,
    thresholds,
    pass: violations.length === 0,
    violations,
    details,
  };
}

export function emitEvaluation(evaluation, environment = process.env, enforce = true) {
  const serialized = JSON.stringify(evaluation);
  console.log(`AWAKEN_LLM_EVAL ${serialized}`);
  const directory = environment.AWAKEN_EVAL_ARTIFACT_DIR;
  if (directory) {
    fs.mkdirSync(directory, { recursive: true });
    const safeSuite = evaluation.suite.replaceAll(/[^a-zA-Z0-9._-]/gu, '_');
    fs.writeFileSync(path.join(directory, `${safeSuite}.json`), `${JSON.stringify(evaluation, null, 2)}\n`);
  }
  if (enforce) {
    assert.ok(evaluation.pass, `LLM evaluation thresholds failed: ${JSON.stringify(evaluation.violations)}`);
  }
}
