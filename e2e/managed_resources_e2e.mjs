// Real-model Managed Agents RESOURCE e2e (ADR-0038): drive a session through the
// official Anthropic TypeScript SDK against awaken-server in `real` mode, and
// prove the resource plane end-to-end with a live model across all three families:
//   - File (stateful, read): `client.beta.files.upload` stores bytes; a session
//     `resources[{type:"file"}]` realizes them into the sandbox; the model reads it.
//   - Artifact (harvest, write): the model writes under `outputs/`; the host harvests
//     it; we retrieve it via `files.list(scope_id)` + `files.download`.
//   - Memory (stateful, read+write, cross-session): a memory store has a stable id;
//     session A writes through the governed mount, and a *new* session B reads
//     the persisted note — proving write-back and cross-session persistence.
// Prompt effect (A3a): the model only learns WHERE each resource lives from the system
// prompt the host injected from the binding — so a correct read/write proves the
// prompt reached the model.
//
// Run (from e2e/, with the KIMI key — note the base URL ends in `/v1/`, since the
// executor posts to `{base_url}messages`):
//   ANTHROPIC_API_KEY=sk-kimi-... ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-k2-0905-preview node managed_resources_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withServer } from './harness.mjs';
import { loadKimiConfig } from './kimi_config.mjs';
import {
  aggregateUsage,
  emitEvaluation,
  makeEvaluation,
  percentile,
  taggedFactMetrics,
} from './llm_eval_metrics.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
// Distinctive test markers, not credentials.
const TOKEN = 'ZEBRA_QUASAR_4718'; // awaken-allow: secret
const ARTIFACT = 'DONE_9931'; // awaken-allow: secret
const MEMTOKEN = 'AWKFACT_NATIVE_MEMORY_MOSS_ORBIT_5527'; // awaken-allow: secret

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function listEvents(client, sid) {
  const evs = [];
  for await (const e of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(e);
  return evs;
}

async function send(client, sid, text) {
  return client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

// Send a user.message, tolerating a thread that is awaiting awaiting a tool decision:
// the server rejects a fresh message while gated, so approve any pending calls and
// retry until it lands (or give up after a bounded number of tries).
async function sendSafe(client, sid, text, approved, confirmationReceiptIds) {
  for (let tries = 0; tries < 10; tries++) {
    try {
      return await send(client, sid, text);
    } catch (e) {
      if (!String(e).includes('awaiting a tool decision')) throw e;
      await approveGated(
        client,
        sid,
        await listEvents(client, sid),
        approved,
        confirmationReceiptIds,
      );
      await sleep(1200);
    }
  }
  return false;
}

// Approve every gated (`evaluated_permission === 'ask'`) tool call not yet approved.
// `write` is not auto-allowed (only read/glob/grep are), so writes await for a
// confirmation — this releases them.
async function approveGated(client, sid, evs, approved, confirmationReceiptIds) {
  for (const e of evs) {
    if (e.type === 'agent.tool_use' && e.evaluated_permission === 'ask' && !approved.has(e.id)) {
      approved.add(e.id);
      // This is an intermediate gate release, not a terminal acceptance oracle;
      // driveUntil receipt-gates the owning prompt after the resulting effects.
      const response = await client.beta.sessions.events.send(sid, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
        betas: BETAS,
      });
      confirmationReceiptIds.push(response.data[0].id);
    }
  }
}

const assistantText = (evs) =>
  evs
    .filter((e) => e.type === 'agent.message')
    .flatMap((m) => (m.content ?? []).map((c) => c.text ?? ''))
    .join(' ');

// Drive one instruction to completion against a real (non-deterministic) model:
// send `text`, approve any gated tool calls each round, and wait until `check()`
// returns true. `check` is re-run every round (it may inspect the assistant's reply
// or external host state such as files.list / a memory store). If the model goes idle
// without satisfying `check`, re-send a firmer `nudgeText` (up to `nudges` times).
// Returns { approved, ok }.
async function driveUntil(client, sid, text, check, { nudges = 2, rounds = 16, nudgeText } = {}) {
  const approved = new Set();
  const confirmationReceiptIds = [];
  const processedConfirmationIds = new Set();
  const waitForConfirmations = async () => {
    for (const receiptId of confirmationReceiptIds) {
      if (processedConfirmationIds.has(receiptId)) continue;
      await waitForSessionEventReceipt(
        client,
        sid,
        receiptId,
        BETAS,
        () => true,
        'resource permission receipt to process',
        { timeoutMs: 120_000, pollMs: 500 },
      );
      processedConfirmationIds.add(receiptId);
    }
  };
  let lastReceipt;
  // Driver decision R1: C1 an exact prompt/nudge receipt, C2 optional permission
  // releases, and C3 the scenario event/external-state predicate succeeds.
  // Effects: E1 intermediate gates advance; E2 C1 is processed while C3 remains
  // true. K1 pre-receipt history may drive gates but cannot complete the turn.
  // R1=C1+C3=>E2; R2=C1+C2+C3=>E1+E2.
  for (let attempt = 0; attempt <= nudges; attempt++) {
    // A nudge is a fresh user.message; `sendSafe` approves any pending gated call and
    // retries so an awaiting thread ("awaiting a tool decision") still accepts it.
    if (await check(await listEvents(client, sid))) return { approved, ok: true };
    const sendResponse = await sendSafe(
      client,
      sid,
      attempt === 0 ? text : nudgeText ?? text,
      approved,
      confirmationReceiptIds,
    );
    lastReceipt = sendResponse?.data?.[0];
    if (!lastReceipt) continue;
    for (let i = 0; i < rounds; i++) {
      await sleep(1500);
      const evs = await listEvents(client, sid);
      await approveGated(client, sid, evs, approved, confirmationReceiptIds);
      if (await check(evs)) {
        await waitForSessionEventReceipt(
          client,
          sid,
          lastReceipt.id,
          BETAS,
          ({ events }) => check(events),
          `resource turn ${JSON.stringify(attempt === 0 ? text : nudgeText ?? text)}`,
          { timeoutMs: 120_000, pollMs: 500 },
        );
        await waitForConfirmations();
        return { approved, ok: true };
      }
      if (evs.length && evs[evs.length - 1].type === 'session.status_idle') {
        await waitForSessionEventReceipt(
          client,
          sid,
          lastReceipt.id,
          BETAS,
          () => true,
          `resource nudge ${attempt + 1} to settle`,
          { timeoutMs: 120_000, pollMs: 500 },
        );
        await waitForConfirmations();
        break; // idle without success → nudge
      }
    }
  }
  const finalEvents = await listEvents(client, sid);
  const ok = await check(finalEvents);
  if (ok && lastReceipt) {
    await waitForSessionEventReceipt(
      client,
      sid,
      lastReceipt.id,
      BETAS,
      ({ events }) => check(events),
      'final resource turn receipt',
      { timeoutMs: 120_000, pollMs: 500 },
    );
    await waitForConfirmations();
  }
  return { approved, ok };
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
    console.log('SKIP managed_resources_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', 38137, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const memoryOnly = process.env.AWAKEN_EVAL_MEMORY_ONLY === '1';

      // ── 1. FILE resource: upload, mount, and read ──────────────────────────────
      // A host without Namespace/Container support can still run the real-LLM
      // Memory quality gate. It must opt in explicitly: silently downgrading the
      // read-only File case to Workdir would claim an isolation property that the
      // backend cannot enforce.
      if (!memoryOnly) {
      const uploaded = await client.beta.files.upload({
        file: await toFile(Buffer.from(`the secret pass phrase is ${TOKEN}`), 'secret.txt'),
        betas: BETAS,
      });
      assert.ok(uploaded.id, 'files.upload returned an id');
      pass(`file uploaded: ${uploaded.id}`);

      const fileSession = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'file', file_id: uploaded.id, mount_path: '/secret.txt' }],
        betas: BETAS,
      });
      assert.equal(fileSession.type, 'session');
      pass(`session created with a file resource: ${fileSession.id}`);

      // The model only learns the path from the injected system prompt (A3a); the token
      // lives only in the file — so reproducing it proves mount + prompt + read.
      const fileRead = await driveUntil(
        client,
        fileSession.id,
        'A file has been mounted into your sandbox. Read it using your tools and reply ' +
          'with the exact secret pass phrase it contains, and nothing else.',
        (evs) => assistantText(evs).includes(TOKEN),
        { nudgeText: 'Use your file tools to read the mounted path, then reply with the exact pass phrase.' },
      );
      assert.ok(fileRead.ok, 'model must reproduce the mounted file token by reading it');
      pass(`model read the mounted file and reproduced the token: ${TOKEN}`);

      // ── 2. ARTIFACT: model writes under outputs/, host harvests, we retrieve ────
      let artifact = null;
      const artWrite = await driveUntil(
        client,
        fileSession.id,
        `Using your tools, write the exact text ${ARTIFACT} into a new file at the path ` +
          `outputs/result.txt. Reply with "written" when done.`,
        async () => {
          const fs = [];
          for await (const f of client.beta.files.list({ scope_id: fileSession.id, betas: BETAS })) fs.push(f);
          artifact = fs.find((f) => (f.filename ?? '').includes('result.txt')) ?? null;
          return !!artifact;
        },
        { nudgeText: `You must call the write tool to create outputs/result.txt containing ${ARTIFACT}.` },
      );
      assert.ok(artWrite.approved.size > 0, 'the write tool should have awaiting for a confirmation');
      pass(`approved ${artWrite.approved.size} gated tool call(s) via user.tool_confirmation`);
      assert.ok(artWrite.ok && artifact, 'session artifact should be harvested + listed');
      pass(`artifact listed via files.list(scope_id): ${artifact.filename} (${artifact.id.slice(0, 12)})`);
      const resp = await client.beta.files.download(artifact.id, { betas: BETAS });
      const text = await resp.text();
      assert.ok(
        text.includes(ARTIFACT),
        `downloaded artifact must contain ${ARTIFACT}; got ${JSON.stringify(text.slice(0, 120))}`,
      );
      pass(`artifact written by the model, harvested + downloaded by the host: ${ARTIFACT}`);
      }

      // ── 3. MEMORY: write-back + cross-session read ─────────────────────────────
      // The memory-store endpoints have no typed SDK binding, so we drive them via the
      // SDK's low-level `client.post` / `client.get` (still the TS SDK).
      const mem = await client.post('/v1/memory_stores', {
        body: { name: 'managed real-model memory evaluation' },
        headers: MEMORY_HEADERS,
      });
      assert.ok(mem.id, 'POST /v1/memory_stores returned an id');
      pass(`memory store created: ${mem.id}`);

      const sessionA = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
        betas: BETAS,
      });
      let memContent = '';
      const memoryWriteStarted = performance.now();
      const memWrite = await driveUntil(
        client,
        sessionA.id,
        `Your sandbox has a memory directory mounted at /mnt/memory. ` +
          `Using your write tool, write exactly /mnt/memory/note.md so ` +
          `its entire contents become: ${MEMTOKEN}. Do not create any other file and do ` +
          `not use an absolute path. Reply with "saved" when done.`,
        async () => {
          // The Memory API is a read-only observation here. The governed mount
          // writes through to the same repository; Files GET has no hidden write edge.
          // Content is observable only in the authoritative full view; the
          // default basic projection intentionally owns metadata alone.
          const page = await client.get(`/v1/memory_stores/${mem.id}/memories?view=full`, {
            headers: MEMORY_HEADERS,
          });
          memContent = (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
          return memContent.includes(MEMTOKEN);
        },
        { nudgeText: `Use the write tool to save the exact text ${MEMTOKEN} into your persistent memory file.` },
      );
      assert.ok(memWrite.approved.size > 0, 'the memory write should have awaiting for a confirmation');
      const writerEvents = await listEvents(client, sessionA.id);
      if (!memWrite.ok) {
        console.error('Memory writer events:', JSON.stringify(writerEvents.map((event) => ({
          type: event.type,
          name: event.name,
          input: event.input,
          error: event.error,
          stop_reason: event.stop_reason,
        }))));
      }
      assert.ok(memWrite.ok, `memory store must hold the written note; got ${JSON.stringify(memContent.slice(0, 120))}`);
      const memoryWriteLatency = performance.now() - memoryWriteStarted;
      pass(`model wrote through the governed Memory mount: ${MEMTOKEN}`);

      // Session B: a fresh session mounts the SAME memory id — the note must be there.
      const sessionB = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
        betas: BETAS,
      });
      const memoryReadStarted = performance.now();
      const memRead = await driveUntil(
        client,
        sessionB.id,
        'Read your persistent memory with your tools and reply with the exact note it ' +
          'contains, and nothing else.',
        (evs) => assistantText(evs).includes(MEMTOKEN),
        { nudgeText: 'Use your file tools to read the mounted memory path, then reply with its exact contents.' },
      );
      assert.ok(memRead.ok, 'session B must read the note persisted by session A');
      const memoryReadLatency = performance.now() - memoryReadStarted;
      pass(`new session read the persisted memory note back: ${MEMTOKEN}`);

      const readerEvents = await listEvents(client, sessionB.id);
      const quality = taggedFactMetrics({ expected: [MEMTOKEN], text: assistantText(readerEvents) });
      const usage = aggregateUsage([...writerEvents, ...readerEvents]);
      emitEvaluation(makeEvaluation({
        suite: 'managed_memory_real_eval_native',
        subject: 'memory-write-through-and-cross-session-recall',
        backend: 'native',
        model: process.env.ANTHROPIC_MODEL ?? process.env.KIMI_MODEL ?? 'provider-default',
        sampleSize: 1,
        metrics: {
          write_commit_rate: memContent.includes(MEMTOKEN) ? 1 : 0,
          cross_session_recall: quality.recall,
          fact_precision: quality.precision,
          fact_f1: quality.f1,
          contamination_rate: quality.contamination_rate,
          approval_enforcement_rate: memWrite.approved.size > 0 ? 1 : 0,
          latency_p50_ms: percentile([memoryWriteLatency, memoryReadLatency], 0.5),
          latency_p95_ms: percentile([memoryWriteLatency, memoryReadLatency], 0.95),
          usage_observed_rate: usage.observed ? 1 : 0,
          input_tokens: usage.input_tokens,
          output_tokens: usage.output_tokens,
        },
        thresholds: {
          write_commit_rate: { operator: 'gte', value: 1 },
          cross_session_recall: { operator: 'gte', value: 1 },
          fact_precision: { operator: 'gte', value: 1 },
          fact_f1: { operator: 'gte', value: 1 },
          contamination_rate: { operator: 'lte', value: 0 },
          approval_enforcement_rate: { operator: 'gte', value: 1 },
          latency_p95_ms: {
            operator: 'lte',
            value: Number(process.env.AWAKEN_MEMORY_MAX_LATENCY_MS ?? 180_000),
          },
        },
        details: {
          writer_session_id: sessionA.id,
          reader_session_id: sessionB.id,
          matched: quality.matched,
          missing: quality.missing,
          unknown: quality.unknown,
        },
      }));
    });

    console.log(process.env.AWAKEN_EVAL_MEMORY_ONLY === '1'
      ? 'E2E PASS: memory write-back/cross-session read verified end-to-end with a real model via the official SDK.'
      : 'E2E PASS: file read + artifact write/retrieve + memory write-back/cross-session read verified end-to-end with a real model via the official SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
