// Live Dream quality evaluation through the official Anthropic SDK.
//
// Cause/effect graph:
//   immutable source Memory (stable + stale fact) ---+
//                                                    +--> live Dream Agent
//   newer committed Session facts ------------------+        |
//                                                            v
//   isolated output Memory --> deterministic fact scorer --> threshold gate
//
// The randomized AWKFACT tokens are semantic canaries. The live model must
// consolidate them; a deterministic scorer checks retention, supersession,
// contamination, source immutability, lifecycle, latency, and token telemetry.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { pass, withServer } from './harness.mjs';
import { loadKimiConfig } from './kimi_config.mjs';
import {
  aggregateUsage,
  emitEvaluation,
  makeEvaluation,
  taggedFactMetrics,
  usageWithFallback,
} from './llm_eval_metrics.mjs';

const BETAS = ['managed-agents-2026-04-01'];
// The Dream worker mounts private copies, making Local a sound portable default.
// CI/operator lanes can still select namespace/docker/podman/k8s explicitly.
process.env.SESSION_DEPLOYMENT_SANDBOX_TIER ??= 'local';
const PORT = Number(process.env.E2E_PORT ?? 38_486);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const tag = (label) => `AWKFACT_${label}_${crypto.randomBytes(6).toString('hex').toUpperCase()}`;

async function drain(items) {
  const values = [];
  for await (const item of items) values.push(item);
  return values;
}

async function waitForSessionIdle(client, id) {
  for (let attempt = 0; attempt < 600; attempt += 1) {
    const session = await client.beta.sessions.retrieve(id, { betas: BETAS });
    if (session.status === 'idle') return session;
    if (session.status === 'terminated') throw new Error(`source session ${id} terminated`);
    await sleep(250);
  }
  throw new Error(`source session ${id} did not become idle`);
}

async function waitForDream(client, id) {
  for (let attempt = 0; attempt < 1_200; attempt += 1) {
    const dream = await client.beta.dreams.retrieve(id, { betas: BETAS });
    if (['completed', 'failed', 'canceled'].includes(dream.status)) return dream;
    await sleep(500);
  }
  throw new Error(`Dream ${id} did not become terminal`);
}

async function sessionEvents(client, id) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(id, { betas: BETAS })) events.push(event);
  return events;
}

async function main() {
  const kimi = loadKimiConfig();
  if (!process.env.ANTHROPIC_API_KEY && kimi?.anthropicKey) {
    Object.assign(process.env, {
      ANTHROPIC_API_KEY: kimi.anthropicKey,
      ANTHROPIC_BASE_URL: kimi.anthropicBase,
      ANTHROPIC_MODEL: kimi.anthropicModel,
    });
  }
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_dream_real_eval_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  process.env.AWAKEN_MODEL_SOURCE = 'http';

  const stable = tag('STABLE');
  const stale = tag('STALE');
  const replacement = tag('REPLACEMENT');
  const preference = tag('PREFERENCE');
  const expected = [stable, replacement, preference];
  const model = process.env.ANTHROPIC_MODEL ?? process.env.KIMI_MODEL ?? 'provider-default';

  await withServer('dream', PORT, async (baseURL) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL, timeout: 700_000 });
    const sourceStore = await client.beta.memoryStores.create({ name: 'live Dream evaluation source' });
    const original = [
      '# Verified project memory',
      `- Stable identifier: ${stable}`,
      `- Deployment identifier (obsolete): ${stale}`,
    ].join('\n');
    await client.beta.memoryStores.memories.create(sourceStore.id, {
      path: '/MEMORY.md', content: original, view: 'full',
    });

    const sourceSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      initial_events: [{
        type: 'user.message',
        content: [{
          type: 'text',
          text: `Record this verified update verbatim: ${replacement} supersedes obsolete ${stale}. `
            + `Also retain the durable preference ${preference}.`,
        }],
      }],
      betas: BETAS,
    });
    await waitForSessionIdle(client, sourceSession.id);

    const startedAt = performance.now();
    const created = await client.beta.dreams.create({
      inputs: [
        { type: 'memory_store', memory_store_id: sourceStore.id },
        { type: 'sessions', session_ids: [sourceSession.id] },
      ],
      model,
      instructions: 'Preserve every verified AWKFACT token verbatim. Remove the obsolete token that '
        + 'the newer Session explicitly supersedes. Do not invent any AWKFACT token.',
      betas: BETAS,
    });
    const terminal = await waitForDream(client, created.id);
    const latencyMs = performance.now() - startedAt;
    const thresholds = {
      completion_rate: { operator: 'gte', value: 1 },
      fact_precision: { operator: 'gte', value: Number(process.env.AWAKEN_DREAM_MIN_PRECISION ?? 1) },
      fact_recall: { operator: 'gte', value: Number(process.env.AWAKEN_DREAM_MIN_RECALL ?? 0.8) },
      fact_f1: { operator: 'gte', value: Number(process.env.AWAKEN_DREAM_MIN_F1 ?? 0.8) },
      contamination_rate: { operator: 'lte', value: 0 },
      stale_fact_rate: { operator: 'lte', value: 0 },
      source_immutability_rate: { operator: 'gte', value: 1 },
      output_isolation_rate: { operator: 'gte', value: 1 },
      output_mutation_rate: { operator: 'gte', value: 1 },
      latency_ms: { operator: 'lte', value: Number(process.env.AWAKEN_DREAM_MAX_LATENCY_MS ?? 600_000) },
    };
    if (terminal.status !== 'completed') {
      const failedEvents = terminal.session_id
        ? await sessionEvents(client, terminal.session_id)
        : [];
      console.error('Dream failure events:', JSON.stringify(failedEvents.map((event) => ({
        type: event.type,
        name: event.name,
        input: event.input,
        error: event.error,
        stop_reason: event.stop_reason,
      }))));
      const source = await drain(client.beta.memoryStores.memories.list(sourceStore.id, { view: 'full' }));
      const sourceText = source.map((memory) => memory.content ?? '').join('\n');
      const usage = usageWithFallback(aggregateUsage(failedEvents), terminal.usage);
      emitEvaluation(makeEvaluation({
        suite: 'managed_dream_real_eval',
        subject: 'dream-consolidation',
        backend: 'native',
        model,
        sampleSize: expected.length,
        metrics: {
          completion_rate: 0,
          fact_precision: 0,
          fact_recall: 0,
          fact_f1: 0,
          contamination_rate: 0,
          stale_fact_rate: 0,
          source_immutability_rate: sourceText === original ? 1 : 0,
          output_isolation_rate: 0,
          output_mutation_rate: 0,
          latency_ms: latencyMs,
          usage_observed_rate: usage.observed ? 1 : 0,
          input_tokens: usage.input_tokens,
          output_tokens: usage.output_tokens,
        },
        thresholds,
        details: {
          dream_id: created.id,
          auxiliary_session_id: terminal.session_id,
          terminal_status: terminal.status,
          terminal_error: terminal.error,
          auxiliary_event_types: failedEvents.map((event) => event.type),
        },
      }), process.env, false);
    }
    assert.equal(terminal.status, 'completed', `Dream failed: ${JSON.stringify(terminal)}`);
    assert.equal(terminal.outputs.length, 1, 'Dream must produce exactly one output MemoryStore');
    const outputStoreID = terminal.outputs[0].memory_store_id;

    const source = await drain(client.beta.memoryStores.memories.list(sourceStore.id, { view: 'full' }));
    const output = await drain(client.beta.memoryStores.memories.list(outputStoreID, { view: 'full' }));
    const sourceText = source.map((memory) => memory.content ?? '').join('\n');
    const outputText = output.map((memory) => memory.content ?? '').join('\n');
    const quality = taggedFactMetrics({ expected, forbidden: [stale], text: outputText });
    const auxiliaryEvents = await sessionEvents(client, terminal.session_id);
    const usage = usageWithFallback(aggregateUsage(auxiliaryEvents), terminal.usage);

    const evaluation = makeEvaluation({
      suite: 'managed_dream_real_eval',
      subject: 'dream-consolidation',
      backend: 'native',
      model,
      sampleSize: expected.length,
      metrics: {
        completion_rate: terminal.status === 'completed' ? 1 : 0,
        fact_precision: quality.precision,
        fact_recall: quality.recall,
        fact_f1: quality.f1,
        contamination_rate: quality.contamination_rate,
        stale_fact_rate: quality.stale_fact_rate,
        source_immutability_rate: sourceText === original ? 1 : 0,
        output_isolation_rate: outputStoreID !== sourceStore.id ? 1 : 0,
        output_mutation_rate: outputText !== original ? 1 : 0,
        latency_ms: latencyMs,
        usage_observed_rate: usage.observed ? 1 : 0,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
      },
      thresholds,
      details: {
        dream_id: created.id,
        source_session_id: sourceSession.id,
        auxiliary_session_id: terminal.session_id,
        managed_model_reference: model,
        matched: quality.matched,
        missing: quality.missing,
        forbidden_matched: quality.forbidden_matched,
        unknown: quality.unknown,
        auxiliary_event_types: auxiliaryEvents.map((event) => event.type),
        auxiliary_tool_calls: auxiliaryEvents
          .filter((event) => event.type === 'agent.tool_use')
          .map((event) => ({ name: event.name, input: event.input })),
        auxiliary_assistant_text: auxiliaryEvents
          .filter((event) => event.type === 'agent.message')
          .flatMap((event) => (event.content ?? []).map((block) => block.text ?? ''))
          .join(' ')
          .slice(0, 1_000),
      },
    });
    emitEvaluation(evaluation);
    pass('live Dream consolidation met lifecycle, quality, isolation and latency thresholds');
  });
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
