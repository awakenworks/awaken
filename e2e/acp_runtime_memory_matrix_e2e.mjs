// Real ACP runtime × durable MemoryStore matrix.
//
// Kimi Code, OpenCode, Claude Code, and Hermes run as real ACP agents against
// the same Kimi model. Servers restart between runtimes over one storage root.
// Every runtime must read the marker written by its predecessor through the
// mounted MemoryStore, then write its own distinct memory file. A second pass
// makes every runtime recall every runtime's file (4 writers × 4 readers). The
// Memory API is observation only: all subject writes/reads happen through
// `.mnt/memory`.
//
// Run:
//   CARGO_TARGET_DIR=/tmp/awaken-memory-runtime-target \
//   node e2e/acp_runtime_memory_matrix_e2e.mjs

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { loadKimiConfig } from './kimi_config.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const RUNTIMES = (process.env.ACP_RUNTIMES ?? 'kimi,opencode,claude,hermes')
  .split(',')
  .map((runtime) => runtime.trim())
  .filter(Boolean);
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

async function approveGated(client, sessionId, events, approved) {
  for (const event of events) {
    if (
      event.type === 'agent.tool_use'
      && event.evaluated_permission === 'ask'
      && !approved.has(event.id)
    ) {
      approved.add(event.id);
      await client.beta.sessions.events.send(sessionId, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: event.id, result: 'allow' }],
        betas: BETAS,
      });
    }
  }
}

async function send(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function driveUntil(client, sessionId, text, check) {
  const approved = new Set();
  // A turn that reaches an `ask` permission remains open until the confirmation
  // arrives. Poll concurrently with the original send; awaiting send first would
  // deadlock exactly on the write/edit operations this matrix must exercise.
  let sendError = null;
  let sendDone = false;
  const sending = send(client, sessionId, text).catch((error) => {
    sendError = error;
  }).finally(() => {
    sendDone = true;
  });
  for (let round = 0; round < 240; round += 1) {
    await sleep(500);
    const events = await listEvents(client, sessionId);
    await approveGated(client, sessionId, events, approved);
    if (sendError) throw sendError;
    if (await check(events)) {
      await sending;
      if (sendError) throw sendError;
      return { events: await listEvents(client, sessionId), approved };
    }
    if (sendDone && events.some((event) => event.type === 'session.status_idle')) {
      return { events, approved };
    }
  }
  return { events: await listEvents(client, sessionId), approved };
}

async function memoryContent(client, storeId) {
  const page = await client.get(`/v1/memory_stores/${storeId}/memories`, {
    headers: MEMORY_HEADERS,
  });
  return (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
}

function configureRuntime(runtime, kimi) {
  Object.assign(process.env, {
    AWAKEN_ACP_CLI: runtime,
    AWAKEN_MODEL: runtime === 'claude' ? kimi.anthropicModel : kimi.openaiModel,
    ANTHROPIC_BASE_URL: kimi.anthropicBase,
    ANTHROPIC_API_KEY: kimi.anthropicKey ?? kimi.key,
    ANTHROPIC_MODEL: kimi.anthropicModel,
  });
  if (runtime === 'kimi') {
    Object.assign(process.env, {
      KIMI_MODEL_BASE_URL: kimi.openaiBase,
      KIMI_MODEL_API_KEY: kimi.key,
      KIMI_MODEL_NAME: kimi.openaiModel,
    });
  } else if (runtime === 'opencode' || runtime === 'codex') {
    Object.assign(process.env, {
      OPENAI_BASE_URL: kimi.openaiBase,
      OPENAI_API_KEY: kimi.key,
      OPENAI_MODEL: kimi.openaiModel,
    });
    if (runtime === 'opencode') {
      Object.assign(process.env, {
      OPENCODE_CONFIG_CONTENT: JSON.stringify({
        model: `awaken-kimi/${kimi.openaiModel}`,
        small_model: `awaken-kimi/${kimi.openaiModel}`,
        enabled_providers: ['awaken-kimi'],
        provider: {
          'awaken-kimi': {
            npm: '@ai-sdk/openai-compatible',
            name: 'Awaken Kimi Code',
            options: { baseURL: kimi.openaiBase, apiKey: '{env:OPENAI_API_KEY}' }, // awaken-allow: secret
            models: { [kimi.openaiModel]: { name: kimi.openaiModel } },
          },
        },
      }),
      });
    }
  } else if (runtime === 'hermes') {
    Object.assign(process.env, {
      KIMI_BASE_URL: kimi.openaiBase,
      KIMI_API_KEY: kimi.key,
      HERMES_MODEL: kimi.openaiModel,
    });
  }
}

async function main() {
  const kimi = loadKimiConfig();
  if (!kimi) {
    console.log('SKIP: no KIMI configuration found in ~/.bashrc');
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
    AWAKEN_SANDBOX_DIR: sandboxDir,
    AWAKEN_SANDBOX_TIER: 'local',
  });

  const chain = RUNTIMES.map((runtime) => ({
    runtime,
    marker: `${runtime}-${crypto.randomBytes(10).toString('hex')}`,
  }));
  const seed = `seed-${crypto.randomBytes(10).toString('hex')}`;
  let storeId = null;

  try {
    for (let index = 0; index < chain.length; index += 1) {
      const { runtime, marker } = chain[index];
      const predecessor = index === 0 ? { runtime: 'seed', marker: seed } : chain[index - 1];
      configureRuntime(runtime, kimi);
      await withServer('acp-real-mcp', 38240 + index, async (baseUrl) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });
        const selectedModel = runtime === 'claude' ? kimi.anthropicModel : kimi.openaiModel;
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
            model: selectedModel,
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
        for (let attempt = 1; attempt <= 3; attempt += 1) {
          readSession = await createSession();
          read = await driveUntil(
            client,
            readSession.id,
            `Read .mnt/memory/${predecessor.runtime}.md with your file tools and reply with only its exact contents.`,
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
            })))}`,
        );
        pass(`${runtime} read the predecessor's MemoryStore marker`);
        await client.beta.sessions.delete(readSession.id, { betas: BETAS });

        let writeSession = await createSession();
        // Keep the MemoryStore matrix independent from the Managed HITL state
        // machine: Claude Code officially supports acceptEdits in settings.json.
        // This file is scoped to this isolated Session config home; the separate
        // ACP permission regression covers ask/cancel/resume behavior.
        configureSession(writeSession);
        let write;
        for (let attempt = 1; attempt <= 3; attempt += 1) {
          write = await driveUntil(
            client,
            writeSession.id,
            `Use your file write tool to create .mnt/memory/${runtime}.md with exactly ${marker}. `
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
        pass(`${runtime} committed its MemoryStore marker (${write.approved.size} approval(s))`);
      });
    }

    // Full cross-runtime recall pass: every reader must observe every writer after
    // the server and sandbox that performed the writes have been torn down.
    for (let index = 0; index < chain.length; index += 1) {
      const { runtime } = chain[index];
      configureRuntime(runtime, kimi);
      await withServer('acp-real-mcp', 38340 + index, async (baseUrl) => {
        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });
        const selectedModel = runtime === 'claude' ? kimi.anthropicModel : kimi.openaiModel;
        const acpAgent = await client.beta.agents.create({
          name: `${runtime} memory reader`,
          model: selectedModel,
          betas: BETAS,
        });
        const paths = chain.map(({ runtime: writer }) => `.mnt/memory/${writer}.md`);
        let session;
        let recalled;
        for (let attempt = 1; attempt <= 3; attempt += 1) {
          session = await client.beta.sessions.create({
            agent: acpAgent.id,
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
        pass(`${runtime} recalled all ${chain.length} runtime-authored memories`);
        await client.beta.sessions.delete(session.id, { betas: BETAS });
      });
    }

    console.log(
      `E2E PASS: ${RUNTIMES.length} writers × ${RUNTIMES.length} readers shared one durable MemoryStore.`,
    );
  } finally {
    fs.rmSync(sandboxHome, { recursive: true, force: true });
    fs.rmSync(sandboxDir, { recursive: true, force: true });
    fs.rmSync(storageDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
