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
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
// Distinctive test markers, not credentials.
const TOKEN = 'ZEBRA_QUASAR_4718'; // awaken-allow: secret
const ARTIFACT = 'DONE_9931'; // awaken-allow: secret
const MEMTOKEN = 'MOSS_ORBIT_5527'; // awaken-allow: secret

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function listEvents(client, sid) {
  const evs = [];
  for await (const e of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(e);
  return evs;
}

async function send(client, sid, text) {
  await client.beta.sessions.events.send(sid, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

// Send a user.message, tolerating a thread that is awaiting awaiting a tool decision:
// the server rejects a fresh message while gated, so approve any pending calls and
// retry until it lands (or give up after a bounded number of tries).
async function sendSafe(client, sid, text, approved) {
  for (let tries = 0; tries < 10; tries++) {
    try {
      await send(client, sid, text);
      return true;
    } catch (e) {
      if (!String(e).includes('awaiting a tool decision')) throw e;
      await approveGated(client, sid, await listEvents(client, sid), approved);
      await sleep(1200);
    }
  }
  return false;
}

// Approve every gated (`evaluated_permission === 'ask'`) tool call not yet approved.
// `write` is not auto-allowed (only read/glob/grep are), so writes await for a
// confirmation — this releases them.
async function approveGated(client, sid, evs, approved) {
  for (const e of evs) {
    if (e.type === 'agent.tool_use' && e.evaluated_permission === 'ask' && !approved.has(e.id)) {
      approved.add(e.id);
      await client.beta.sessions.events.send(sid, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
        betas: BETAS,
      });
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
  for (let attempt = 0; attempt <= nudges; attempt++) {
    // A nudge is a fresh user.message; `sendSafe` approves any pending gated call and
    // retries so an awaiting thread ("awaiting a tool decision") still accepts it.
    if (await check(await listEvents(client, sid))) return { approved, ok: true };
    await sendSafe(client, sid, attempt === 0 ? text : nudgeText ?? text, approved);
    for (let i = 0; i < rounds; i++) {
      await sleep(1500);
      const evs = await listEvents(client, sid);
      await approveGated(client, sid, evs, approved);
      if (await check(evs)) return { approved, ok: true };
      if (evs.length && evs[evs.length - 1].type === 'session.status_idle') {
        if (await check(evs)) return { approved, ok: true };
        break; // idle without success → nudge
      }
    }
  }
  return { approved, ok: await check(await listEvents(client, sid)) };
}

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_resources_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', 38137, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // ── 1. FILE resource: upload, mount, and read ──────────────────────────────
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

      // ── 3. MEMORY: write-back + cross-session read ─────────────────────────────
      // The memory-store endpoints have no typed SDK binding, so we drive them via the
      // SDK's low-level `client.post` / `client.get` (still the TS SDK).
      const mem = await client.post('/v1/memory_stores');
      assert.ok(mem.id, 'POST /v1/memory_stores returned an id');
      pass(`memory store created: ${mem.id}`);

      const sessionA = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
        betas: BETAS,
      });
      let memContent = '';
      const memWrite = await driveUntil(
        client,
        sessionA.id,
        `Your sandbox has a memory directory mounted at .mnt/memory. ` +
          `Using your write tool, write exactly .mnt/memory/note.md so ` +
          `its entire contents become: ${MEMTOKEN}. Do not create any other file and do ` +
          `not use an absolute path. Reply with "saved" when done.`,
        async () => {
          // The Memory API is a read-only observation here. The governed mount
          // writes through to the same repository; Files GET has no hidden write edge.
          const page = await client.get(`/v1/memory_stores/${mem.id}/memories`);
          memContent = (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
          return memContent.includes(MEMTOKEN);
        },
        { nudgeText: `Use the write tool to save the exact text ${MEMTOKEN} into your persistent memory file.` },
      );
      assert.ok(memWrite.approved.size > 0, 'the memory write should have awaiting for a confirmation');
      assert.ok(memWrite.ok, `memory store must hold the written note; got ${JSON.stringify(memContent.slice(0, 120))}`);
      pass(`model wrote through the governed Memory mount: ${MEMTOKEN}`);

      // Session B: a fresh session mounts the SAME memory id — the note must be there.
      const sessionB = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
        betas: BETAS,
      });
      const memRead = await driveUntil(
        client,
        sessionB.id,
        'Read your persistent memory with your tools and reply with the exact note it ' +
          'contains, and nothing else.',
        (evs) => assistantText(evs).includes(MEMTOKEN),
        { nudgeText: 'Use your file tools to read the mounted memory path, then reply with its exact contents.' },
      );
      assert.ok(memRead.ok, 'session B must read the note persisted by session A');
      pass(`new session read the persisted memory note back: ${MEMTOKEN}`);
    });

    console.log(
      'E2E PASS: file read + artifact write/retrieve + memory write-back/cross-session read verified end-to-end with a real model via the official SDK.',
    );
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
