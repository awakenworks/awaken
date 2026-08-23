// Real ACP runtime × durable MemoryStore matrix.
//
// OpenCode, Claude Code, and Hermes run as real ACP agents against the same
// Kimi model. Servers restart between runtimes over one storage root.
// Every runtime must read the marker written by its predecessor through the
// mounted MemoryStore, then write its own distinct memory file. A second pass
// makes every runtime recall every runtime's file (N writers × N readers). The
// Memory API is observation only: all subject writes/reads happen through
// `/mnt/memory`.
//
// Run:
//   CARGO_TARGET_DIR=/tmp/awaken-memory-runtime-target \
//   node e2e/acp_runtime_memory_matrix_e2e.mjs

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  pass,
  waitForSessionEventReceipt,
  withServer,
} from './harness.mjs';
import { loadKimiConfig } from './kimi_config.mjs';
import {
  aggregateUsage,
  emitEvaluation,
  makeEvaluation,
  percentile,
  taggedFactMetrics,
} from './llm_eval_metrics.mjs';
import {
  applyAcpRuntimeProfile,
  assertAcpRuntimeVersions,
  parseAcpRuntimes,
  resolveAcpRuntimeProfiles,
} from './acp_runtime_profiles.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const RUNTIMES = parseAcpRuntimes(process.env.ACP_RUNTIMES);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function listEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

function assistantText(events) {
  return events
    .filter((event) => event.type === 'agent.message')
    .flatMap((message) => (message.content ?? []).map((content) => content.text ?? ''))
    .join(' ');
}

async function approveGated(client, sessionId, events, approved, confirmationReceiptIds) {
  for (const event of events) {
    if (
      event.type === 'agent.tool_use'
      && event.evaluated_permission === 'ask'
      && !approved.has(event.id)
    ) {
      approved.add(event.id);
      const response = await client.beta.sessions.events.send(sessionId, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: event.id, result: 'allow' }],
        betas: BETAS,
      });
      confirmationReceiptIds.push(response.data[0].id);
    }
  }
}

async function send(client, sessionId, text) {
  return client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function driveUntil(client, sessionId, text, check) {
  const approved = new Set();
  const confirmationReceiptIds = [];
  const waitForConfirmations = async (description) => {
    for (const receiptId of confirmationReceiptIds) {
      await waitForSessionEventReceipt(
        client,
        sessionId,
        receiptId,
        BETAS,
        () => true,
        description,
        { timeoutMs: 120_000, pollMs: 500 },
      );
    }
  };
  // A turn that reaches an `ask` permission remains open until the confirmation
  // arrives. Poll concurrently with the original send; awaiting send first would
  // deadlock exactly on the write/edit operations this matrix must exercise.
  let sendError = null;
  let sendDone = false;
  let sendResponse;
  // Driver decision D1: C1 exact prompt receipt, C2 zero-or-more intermediate
  // permission gates, C3 scenario check succeeds. Effects: E1 gates are driven;
  // E2 the exact receipt is processed with C3 still true. K1 polling may observe
  // intermediate state, but older history cannot complete this turn.
  // D1=C1+C3=>E2; D2=C1+C2+C3=>E1+E2.
  const sending = send(client, sessionId, text)
    .then((response) => {
      sendResponse = response;
    })
    .catch((error) => {
      sendError = error;
    })
    .finally(() => {
      sendDone = true;
    });
  for (let round = 0; round < 240; round += 1) {
    await sleep(500);
    const events = await listEvents(client, sessionId);
    await approveGated(client, sessionId, events, approved, confirmationReceiptIds);
    if (sendError) throw sendError;
    if (await check(events)) {
      await sending;
      if (sendError) throw sendError;
      const receipt = sendResponse?.data?.[0];
      const observation = await waitForSessionEventReceipt(
        client,
        sessionId,
        receipt?.id,
        BETAS,
        ({ events: committed }) => check(committed),
        `ACP runtime Memory turn ${JSON.stringify(text)}`,
        { timeoutMs: 120_000, pollMs: 500 },
      );
      await waitForConfirmations('ACP runtime Memory permission receipt to process');
      return { events: observation.events, approved };
    }
    if (sendDone && events.some((event) => event.type === 'session.status_idle')) {
      if (sendError) throw sendError;
      const receipt = sendResponse?.data?.[0];
      const observation = await waitForSessionEventReceipt(
        client,
        sessionId,
        receipt?.id,
        BETAS,
        () => true,
        `ACP runtime Memory turn ${JSON.stringify(text)} to settle`,
        { timeoutMs: 120_000, pollMs: 500 },
      );
      await waitForConfirmations('settled ACP runtime Memory permission receipt');
      return { events: observation.events, approved };
    }
  }
  return { events: await listEvents(client, sessionId), approved };
}

async function memoryContent(client, storeId) {
  // Rule MC1: a content check must select the full projection; basic is a
  // metadata-only success and cannot prove ACP extraction/recall bytes.
  const page = await client.get(`/v1/memory_stores/${storeId}/memories?view=full`, {
    headers: MEMORY_HEADERS,
  });
  return (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
}

async function main() {
  const kimi = loadKimiConfig();
  let profiles;
  let runtimeVersions;
  try {
    profiles = resolveAcpRuntimeProfiles({ runtimes: RUNTIMES, kimi });
    runtimeVersions = assertAcpRuntimeVersions({
      runtimes: RUNTIMES,
      probe: ({ executable, args }) => spawnSync(executable, args, {
        encoding: 'utf8',
        env: process.env,
        timeout: 10_000,
      }),
    });
  } catch (error) {
    if (process.env.ACP_MEMORY_REQUIRE_RUNTIMES === '1') throw error;
    console.log(`SKIP acp_runtime_memory_matrix_e2e: ${error.message}`);
    return;
  }

  const realHome = os.homedir();
  const sandboxHome = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-memory-matrix-home-'));
  const sandboxDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-memory-matrix-sandboxes-'));
  const storageDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-memory-matrix-store-'));
  Object.assign(process.env, {
    CARGO_HOME: process.env.CARGO_HOME ?? path.join(realHome, '.cargo'),
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(realHome, '.rustup'),
    HOME: sandboxHome,
    SESSION_DEPLOYMENT_STORAGE_DIR: storageDir,
    SESSION_DEPLOYMENT_SANDBOX_DIR: sandboxDir,
    AWAKEN_SANDBOX_TIER: 'local',
  });

  const chain = RUNTIMES.map((runtime) => ({
    runtime,
    marker: `AWKFACT_${runtime.toUpperCase().replaceAll(/[^A-Z0-9]/gu, '_')}_${crypto.randomBytes(10).toString('hex').toUpperCase()}`,
  }));
  const seed = `AWKFACT_SEED_${crypto.randomBytes(10).toString('hex').toUpperCase()}`;
  let storeId = null;
  const operationLatencies = [];
  const allEvents = [];
  let predecessorRecallSuccesses = 0;
  let committedWrites = 0;
  let crossRuntimeRecall = 0;
  let crossRuntimePrecision = 0;
  let crossRuntimeF1 = 0;
  let crossRuntimeContamination = 0;
  let retryCount = 0;

  try {
    for (let index = 0; index < chain.length; index += 1) {
      const { runtime, marker } = chain[index];
      const profile = profiles[index];
      const predecessor = index === 0 ? { runtime: 'seed', marker: seed } : chain[index - 1];
      applyAcpRuntimeProfile(profile);
      await withServer('acp-real-mcp', 38240 + index, async (baseUrl) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });
        const selectedModel = profile.model;
        const acpAgent = await client.beta.agents.create({
          name: `${runtime} memory writer`,
          model: selectedModel,
          betas: BETAS,
        });
        if (storeId === null) {
          const store = await client.post('/v1/memory_stores', {
            body: { name: 'cross-runtime-memory' },
            headers: MEMORY_HEADERS,
          });
          storeId = store.id;
          await client.post(`/v1/memory_stores/${storeId}/memories`, {
            body: { path: '/seed.md', content: seed },
            headers: MEMORY_HEADERS,
          });
        }

        const createSession = () => client.beta.sessions.create({
            agent: acpAgent.id,
            environment_id: 'env_local',
            resources: [{
              type: 'memory_store',
              memory_store_id: storeId,
              mount_path: '/memory',
            }],
            betas: BETAS,
          });
        const configureSession = (session) => {
          if (runtime !== 'claude') return;
          const configHome = path.join(sandboxDir, session.id, '.acp-config');
          fs.mkdirSync(configHome, { recursive: true });
          fs.writeFileSync(
            path.join(configHome, 'settings.json'),
            JSON.stringify({ permissions: { defaultMode: 'acceptEdits' } }),
          );
        };

        // Use one ACP process/session per operation. This keeps the matrix focused
        // on shared MemoryStore semantics; multi-turn session/load compatibility
        // has its own protocol regression test.
        let readSession;
        let read;
        let readAttempts = 0;
        const readStarted = performance.now();
        for (let attempt = 1; attempt <= 3; attempt += 1) {
          readAttempts = attempt;
          readSession = await createSession();
          read = await driveUntil(
            client,
            readSession.id,
            `Read /mnt/memory/${predecessor.runtime}.md with your file tools and reply with only its exact contents.`,
            (events) => assistantText(events).includes(predecessor.marker),
          );
          if (assistantText(read.events).includes(predecessor.marker)) break;
          if (attempt < 3) {
            await client.beta.sessions.delete(readSession.id, { betas: BETAS });
            await sleep(2_000 * attempt);
          }
        }
        assert.ok(
          assistantText(read.events).includes(predecessor.marker),
          `${runtime} must read predecessor marker ${predecessor.marker}; `
            + `assistant=${JSON.stringify(assistantText(read.events))}, `
            + `events=${JSON.stringify(read.events.map((event) => ({
              type: event.type,
              tool: event.name ?? event.tool_name,
              permission: event.evaluated_permission,
              error: event.error,
              stop_reason: event.stop_reason,
            })))}`,
        );
        retryCount += readAttempts - 1;
        predecessorRecallSuccesses += 1;
        operationLatencies.push(performance.now() - readStarted);
        allEvents.push(...read.events);
        pass(`${runtime} read the predecessor's MemoryStore marker`);
        await client.beta.sessions.delete(readSession.id, { betas: BETAS });

        let writeSession = await createSession();
        // Keep the MemoryStore matrix independent from the Managed HITL state
        // machine: Claude Code officially supports acceptEdits in settings.json.
        // This file is scoped to this isolated Session config home; the separate
        // ACP permission regression covers ask/cancel/resume behavior.
        configureSession(writeSession);
        let write;
        let writeAttempts = 0;
        const writeStarted = performance.now();
        for (let attempt = 1; attempt <= 3; attempt += 1) {
          writeAttempts = attempt;
          write = await driveUntil(
            client,
            writeSession.id,
            `Use your file write tool to create /mnt/memory/${runtime}.md with exactly ${marker}. `
              + 'Do not write any other content. Reply with only SAVED.',
            (events) => assistantText(events).includes('SAVED'),
          );
          if (assistantText(write.events).includes('SAVED')) break;
          if (attempt < 3) {
            await client.beta.sessions.delete(writeSession.id, { betas: BETAS });
            await sleep(2_000 * attempt);
            writeSession = await createSession();
            configureSession(writeSession);
          }
        }
        assert.ok(
          assistantText(write.events).includes('SAVED'),
          `${runtime} must report completion after writing its mounted MemoryStore copy; `
            + `assistant=${JSON.stringify(assistantText(write.events))}`,
        );
        // Copy-backed writable mounts commit at the Session teardown boundary.
        // FUSE may expose the write sooner, but the portable contract is that
        // DELETE disposes the environment and harvests its authoritative copy.
        await client.beta.sessions.delete(writeSession.id, { betas: BETAS });
        let committed = false;
        for (let round = 0; round < 40; round += 1) {
          if ((await memoryContent(client, storeId)).includes(marker)) {
            committed = true;
            break;
          }
          await sleep(250);
        }
        assert.ok(
          committed,
          `${runtime} must commit marker ${marker} when its Session is disposed`,
        );
        retryCount += writeAttempts - 1;
        committedWrites += 1;
        operationLatencies.push(performance.now() - writeStarted);
        allEvents.push(...write.events);
        pass(`${runtime} committed its MemoryStore marker (${write.approved.size} approval(s))`);
      });
    }

    // Full cross-runtime recall pass: every reader must observe every writer after
    // the server and sandbox that performed the writes have been torn down.
    for (let index = 0; index < chain.length; index += 1) {
      const { runtime } = chain[index];
      const profile = profiles[index];
      applyAcpRuntimeProfile(profile);
      await withServer('acp-real-mcp', 38340 + index, async (baseUrl) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });
        const selectedModel = profile.model;
        const acpAgent = await client.beta.agents.create({
          name: `${runtime} memory reader`,
          model: selectedModel,
          betas: BETAS,
        });
        const paths = chain.map(({ runtime: writer }) => `/mnt/memory/${writer}.md`);
        let session;
        let recalled;
        let recallAttempts = 0;
        const recallStarted = performance.now();
        for (let attempt = 1; attempt <= 3; attempt += 1) {
          recallAttempts = attempt;
          session = await client.beta.sessions.create({
            agent: acpAgent.id,
            environment_id: 'env_local',
            model: selectedModel,
            resources: [{
              type: 'memory_store',
              memory_store_id: storeId,
              mount_path: '/memory',
            }],
            betas: BETAS,
          });
          recalled = await driveUntil(
            client,
            session.id,
            `Read all of these memory files with your file tools: ${paths.join(', ')}. `
              + 'Reply with their exact contents, one per line, and no other text.',
            (events) => chain.every(({ marker }) => assistantText(events).includes(marker)),
          );
          if (chain.every(({ marker }) => assistantText(recalled.events).includes(marker))) break;
          await client.beta.sessions.delete(session.id, { betas: BETAS });
          if (attempt < 3) await sleep(2_000 * attempt);
        }
        const text = assistantText(recalled.events);
        for (const { runtime: writer, marker } of chain) {
          assert.ok(
            text.includes(marker),
            `${runtime} must recall ${writer}'s marker ${marker}; assistant=${JSON.stringify(text)}`,
          );
        }
        const quality = taggedFactMetrics({
          expected: chain.map(({ marker }) => marker),
          text,
        });
        retryCount += recallAttempts - 1;
        crossRuntimeRecall += quality.recall;
        crossRuntimePrecision += quality.precision;
        crossRuntimeF1 += quality.f1;
        crossRuntimeContamination += quality.contamination_rate;
        operationLatencies.push(performance.now() - recallStarted);
        allEvents.push(...recalled.events);
        pass(`${runtime} recalled all ${chain.length} runtime-authored memories`);
        await client.beta.sessions.delete(session.id, { betas: BETAS });
      });
    }

    const usage = aggregateUsage(allEvents);
    emitEvaluation(makeEvaluation({
      suite: 'managed_memory_real_eval_acp_runtime_matrix',
      subject: 'durable-memory-cross-runtime-recall',
      backend: `acp:${RUNTIMES.join(',')}`,
      model: kimi.openaiModel,
      sampleSize: chain.length * chain.length,
      metrics: {
        write_commit_rate: committedWrites / chain.length,
        predecessor_recall_rate: predecessorRecallSuccesses / chain.length,
        cross_runtime_recall_at_k: crossRuntimeRecall / chain.length,
        fact_precision: crossRuntimePrecision / chain.length,
        fact_f1: crossRuntimeF1 / chain.length,
        contamination_rate: crossRuntimeContamination / chain.length,
        latency_p50_ms: percentile(operationLatencies, 0.5),
        latency_p95_ms: percentile(operationLatencies, 0.95),
        retry_rate: retryCount / (chain.length * 3),
        usage_observed_rate: usage.observed ? 1 : 0,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
      },
      thresholds: {
        write_commit_rate: { operator: 'gte', value: 1 },
        predecessor_recall_rate: { operator: 'gte', value: 1 },
        cross_runtime_recall_at_k: { operator: 'gte', value: 1 },
        fact_precision: { operator: 'gte', value: 1 },
        fact_f1: { operator: 'gte', value: 1 },
        contamination_rate: { operator: 'lte', value: 0 },
        latency_p95_ms: {
          operator: 'lte',
          value: Number(process.env.AWAKEN_ACP_MEMORY_MAX_LATENCY_MS ?? 600_000),
        },
      },
      details: {
        runtimes: RUNTIMES,
        credential_profiles: profiles.map(({ runtime, auth }) => ({ runtime, auth })),
        runtime_versions: runtimeVersions,
        writers: chain.length,
        readers: chain.length,
        retries: retryCount,
        usage_observed: usage.observed,
      },
    }));
    console.log(
      `E2E PASS: ${RUNTIMES.length} writers × ${RUNTIMES.length} readers shared one durable MemoryStore.`,
    );
  } finally {
    fs.rmSync(sandboxHome, { recursive: true, force: true });
    // Every runtime may leave a live or disconnected projection after its
    // process boundary. The canonical cleanup decision table detaches those
    // mounts before removing either owner tree and fails closed on detach.
    cleanupFixtureTree(sandboxDir);
    cleanupFixtureTree(storageDir);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
