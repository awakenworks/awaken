// Real-model Managed Agents RESOURCE e2e (ADR-0038): drive a session through the
// official Anthropic TypeScript SDK against awaken-server in `real` mode, and
// prove the resource plane end-to-end with a live model across all three families:
//   - File (stateful, read): `client.beta.files.upload` stores bytes; a session
//     `resources[{type:"file"}]` realizes them into the sandbox; the model reads it.
//   - Artifact (harvest, write): the model writes under the runtime-owned
//     `/mnt/session/outputs/` mount; the host harvests
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
import {
  FILES_BETA,
  allowManagedToolBoundaries,
  pass,
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

const BETAS = ['managed-agents-2026-04-01', FILES_BETA];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
// Distinctive test markers, not credentials.
const TOKEN = 'ZEBRA_QUASAR_4718'; // awaken-allow: secret
const ARTIFACT = 'DONE_9931'; // awaken-allow: secret
const MEMTOKEN = 'AWKFACT_NATIVE_MEMORY_MOSS_ORBIT_5527'; // awaken-allow: secret

const assistantText = (evs) =>
  evs
    .filter((e) => e.type === 'agent.message')
    .flatMap((m) => (m.content ?? []).map((c) => c.text ?? ''))
    .join(' ');

// Drive one real-model instruction while leaving receipt→approval→end_turn
// sequencing to the canonical harness. Causes: C1 an exact prompt/nudge receipt;
// C2 the harness reaches its receipt-scoped end_turn; C3 the scenario-specific
// event/external-state check succeeds; C4 nudge budget remains. Effects: E1
// C1+C2+C3 returns terminal history and only confirmations owned after C1; E2
// C1+C2+!C3+C4 sends a nudge only after the prior turn is idle; E3
// C1+C2+!C3+!C4 returns ok=false. C2 failure is surfaced by the harness and never
// converted to a nudge. Constraint: every caller creates a fresh uniquely marked
// target, so a preexisting-effect/zero-send partition is not legal here. Rules
// S1=C1+C2+C3=>E1; S2=C1+C2+!C3+C4=>E2; S3=C1+C2+!C3+!C4=>E3.
async function driveUntil(client, sid, text, check, { nudges = 2, nudgeText } = {}) {
  const approved = new Set();
  let events = [];
  for (let attempt = 0; attempt <= nudges; attempt++) {
    const prompt = attempt === 0 ? text : nudgeText ?? text;
    const taskReceipt = (await client.beta.sessions.events.send(sid, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
      betas: BETAS,
    })).data[0];
    assert.equal(typeof taskReceipt?.id, 'string', `S1-S3 exact receipt for ${prompt}`);
    events = await allowManagedToolBoundaries({
      client,
      sessionId: sid,
      taskReceiptId: taskReceipt.id,
      betas: BETAS,
      description: `resource turn ${JSON.stringify(prompt)}`,
      timeoutMs: 120_000,
      maxBoundaries: 20,
    });
    const receiptIndex = events.findIndex((event) => event.id === taskReceipt.id);
    assert.notEqual(receiptIndex, -1, 'S1-S3 terminal history retains its task receipt');
    for (const event of events.slice(receiptIndex + 1)) {
      if (event.type === 'user.tool_confirmation' && event.result === 'allow') {
        approved.add(event.tool_use_id);
      }
    }
    if (await check(events)) return { approved, ok: true, events };
  }
  return { approved, ok: false, events };
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

      // ── 2. ARTIFACT: model writes under the runtime output mount, host harvests ──
      let artifact = null;
      const artWrite = await driveUntil(
        client,
        fileSession.id,
        `Using your tools, write the exact text ${ARTIFACT} into a new file at the path ` +
          `/mnt/session/outputs/result.txt. Reply with "written" when done.`,
        async () => {
          const fs = [];
          for await (const f of client.beta.files.list({ scope_id: fileSession.id, betas: BETAS })) fs.push(f);
          artifact = fs.find((f) => (f.filename ?? '').includes('result.txt')) ?? null;
          return !!artifact;
        },
        { nudgeText: `You must call the write tool to create /mnt/session/outputs/result.txt containing ${ARTIFACT}.` },
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
        resources: [{ type: 'memory_store', memory_store_id: mem.id }],
        betas: BETAS,
      });
      // Memory path decision rule: C1 the official id-only input names this
      // exact Store; E1 Session create returns the catalog-owned frozen mount.
      // The live prompt consumes E1 and never reimplements display-name slugging.
      const memoryResource = sessionA.resources.find(
        (resource) => resource.type === 'memory_store' && resource.memory_store_id === mem.id,
      );
      assert.equal(
        typeof memoryResource?.mount_path,
        'string',
        'Session create returns the exact catalog-derived MemoryStore mount',
      );
      const memoryNotePath = `${memoryResource.mount_path}/note.md`;
      let memContent = '';
      const memoryWriteStarted = performance.now();
      const memWrite = await driveUntil(
        client,
        sessionA.id,
        `Your sandbox has a persistent memory directory mounted at ${memoryResource.mount_path}. ` +
          `Using your write tool, write exactly ${memoryNotePath} so ` +
          `its entire contents become: ${MEMTOKEN}. Do not create any other file and do ` +
          `not write anywhere else. Reply with "saved" when done.`,
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
      const writerEvents = memWrite.events;
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
        resources: [{ type: 'memory_store', memory_store_id: mem.id }],
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

      const readerEvents = memRead.events;
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
