// Managed Dream cross-module E2E driven only through the official Anthropic
// TypeScript SDK. The test owns the Dreams SDK behavior surface; lower-level
// JSONL/tool-payload and mount failure alternatives remain with their Rust
// decision-table tests instead of being duplicated here.
//
// Cause/effect graph:
//
// Managed Session + committed event -----+
//                                        v
// source MemoryStore -- Dream create -> frozen JSONL + auxiliary Session
//       |                                |             |
//       |                                v             v
//       +-- remains unchanged       independent output terminal lifecycle
//
// Decision table:
//
// | Rule | Inputs/state | Lifecycle operation | Observable effect |
// |------|--------------|---------------------|-------------------|
// | P0 | official SDK + Managed/Dream betas | create | accepted typed Dream |
// | P1 | source store + one completed Session | execute | completed with output/session refs |
// | P2 | committed transcript text | export/cleanup | transient JSONL is purged after use |
// | P3 | prepared result | complete | source unchanged; output has a distinct id and cloned content |
// | P4 | auxiliary execution | terminal | ordinary Session is terminated and marked Dream origin |
// | P5 | terminal Dream | list/archive/cancel | filters/archive work; completed cancel is 400 |
// | P6 | all above through one process boundary | SDK decode | no compatibility shim or raw Dream request |
//
// Causes: official SDK input shape and beta injection; source MemoryStore and
// committed Session state; Dream lifecycle state; terminal mutations.
// Constraints: one source store, one non-running Session, an independently
// addressable output store, and no raw HTTP request for a Dream operation.
// Effects: typed asynchronous creation, transient frozen JSONL, an ordinary
// auxiliary Session, isolated output content, and correct list/cancel/archive
// projections.
// Decision rules: P0-P6 above jointly cover the successful cross-module path
// and every terminal operation exposed by the official Dreams SDK resource.
//
// Run from e2e/: npm run test:dream

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import type { AnthropicBeta } from '@anthropic-ai/sdk/resources/beta/beta';
import type { BetaDream } from '@anthropic-ai/sdk/resources/beta/dreams';
import type { BetaManagedAgentsMemory } from '@anthropic-ai/sdk/resources/beta/memory-stores/memories';
// @ts-ignore -- the shared harness intentionally remains plain JavaScript for
// the repository's Node and TypeScript matrices.
import { pass, withServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38_434);
const MANAGED_BETAS: AnthropicBeta[] = ['managed-agents-2026-04-01'];
// Dream inputs are private snapshots, so the portable Workdir tier preserves
// source isolation without pretending to enforce an external read-only bind.
process.env.SESSION_DEPLOYMENT_SANDBOX_TIER ??= 'local';

async function drain<T>(items: AsyncIterable<T>): Promise<T[]> {
  const drained: T[] = [];
  for await (const item of items) drained.push(item);
  return drained;
}

async function waitForSessionIdle(client: Anthropic, sessionID: string) {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    const session = await client.beta.sessions.retrieve(sessionID, { betas: MANAGED_BETAS });
    if (session.status === 'idle') return session;
    if (session.status === 'terminated') {
      throw new Error(`source Session terminated before becoming idle: ${JSON.stringify(session)}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`source Session ${sessionID} did not become idle`);
}

async function waitForDream(client: Anthropic, dreamID: string): Promise<BetaDream> {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    const dream = await client.beta.dreams.retrieve(dreamID, { betas: MANAGED_BETAS });
    if (['completed', 'failed', 'canceled'].includes(dream.status)) return dream;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`Dream ${dreamID} did not reach a terminal state`);
}

function fullMemories(items: unknown[]): BetaManagedAgentsMemory[] {
  return items.filter(
    (item): item is BetaManagedAgentsMemory =>
      typeof item === 'object' && item !== null && (item as { type?: string }).type === 'memory',
  );
}

async function status(action: () => Promise<unknown>): Promise<number> {
  try {
    await action();
    return 200;
  } catch (error) {
    return error instanceof Anthropic.APIError ? error.status : -1;
  }
}

async function main() {
  await withServer('dream', PORT, async (baseURL: string) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });

    const sourceSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      initial_events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'Remember that Project Atlas uses Rust.' }],
      }],
      betas: MANAGED_BETAS,
    });
    await waitForSessionIdle(client, sourceSession.id);

    const sourceStore = await client.beta.memoryStores.create({ name: 'Dream source' });
    await client.beta.memoryStores.memories.create(sourceStore.id, {
      path: '/MEMORY.md',
      content: '# Existing\n- Keep me.\n',
      view: 'full',
    });

    const beforeInvalid = await drain(client.beta.dreams.list({ betas: MANAGED_BETAS }));
    for (const model of [
      'executor=a2a:https://third-party.example/agent',
      'qwen/qwen3;api=open_ai_chat;executor=acp:opencode',
    ]) {
      assert.equal(await status(() => client.beta.dreams.create({
        inputs: [
          { type: 'memory_store', memory_store_id: sourceStore.id },
          { type: 'sessions', session_ids: [sourceSession.id] },
        ],
        model,
        betas: MANAGED_BETAS,
      })), 400, `P0b unsupported/malformed model reference fails admission: ${model}`);
    }
    const afterInvalid = await drain(client.beta.dreams.list({ betas: MANAGED_BETAS }));
    assert.equal(afterInvalid.length, beforeInvalid.length, 'P0b invalid references persist no Dream');
    pass('P0b malformed qualifiers and outbound A2A Dream references fail before persistence');

    const created = await client.beta.dreams.create({
      inputs: [
        { type: 'memory_store', memory_store_id: sourceStore.id },
        { type: 'sessions', session_ids: [sourceSession.id] },
      ],
      model: 'claude-sonnet-5',
      instructions: 'Retain verified project conventions.',
      betas: MANAGED_BETAS,
    });
    assert.equal(created.type, 'dream', 'P0 official SDK decodes the Dream resource');
    assert.equal(created.status, 'pending', 'P0 create is asynchronous');
    pass('P0 official SDK creates an asynchronous Dream with both beta capabilities');

    const terminal = await waitForDream(client, created.id);
    assert.equal(terminal.status, 'completed', `P1 terminal Dream: ${JSON.stringify(terminal)}`);
    assert.equal(terminal.outputs.length, 1, 'P1 completed Dream has one output');
    assert.ok(terminal.session_id, 'P1 prepared Dream exposes its auxiliary Session');
    const outputStoreID = terminal.outputs[0].memory_store_id;
    assert.notEqual(outputStoreID, sourceStore.id, 'P3 output is an independent MemoryStore');
    pass('P1 Dream completes with stable output and auxiliary Session references');

    const sourceMemories = fullMemories(await drain(
      client.beta.memoryStores.memories.list(sourceStore.id, { view: 'full' }),
    ));
    const outputMemories = fullMemories(await drain(
      client.beta.memoryStores.memories.list(outputStoreID, { view: 'full' }),
    ));
    assert.equal(sourceMemories.length, 1, 'P3 source has one memory');
    assert.equal(outputMemories.length, 1, 'P3 output cloned the source memory');
    assert.equal(sourceMemories[0].content, '# Existing\n- Keep me.\n');
    assert.equal(outputMemories[0].content, '# Dream\n- Consolidated by the Dream Agent.\n');
    pass('P3 source remains unchanged and the independent output contains the tool-written result');

    const auxiliary = await client.beta.sessions.retrieve(terminal.session_id!, {
      betas: MANAGED_BETAS,
    });
    assert.equal(auxiliary.status, 'terminated', 'P4 Dream archives its ordinary auxiliary Session');
    assert.equal(auxiliary.metadata['awaken.session.origin'], 'dream', 'P4 origin is explicit');
    pass('P4 Dream work remains observable as an ordinary terminal Managed Session');

    const files = await drain(client.beta.files.list({ betas: MANAGED_BETAS }));
    const transcript = files.find((file) => file.filename === `${sourceSession.id}.jsonl`);
    assert.equal(transcript, undefined, 'P2 transient JSONL is purged after terminal cleanup');
    pass('P2 transcript Files follow the bounded Dream input lifecycle');

    const completed = await drain(client.beta.dreams.list({
      statuses: ['completed'],
      betas: MANAGED_BETAS,
    }));
    assert.ok(completed.some((dream) => dream.id === created.id), 'P5 status filter includes Dream');
    assert.equal(
      await status(() => client.beta.dreams.cancel(created.id, { betas: MANAGED_BETAS })),
      400,
      'P5 completed Dream cannot be canceled',
    );
    const archived = await client.beta.dreams.archive(created.id, { betas: MANAGED_BETAS });
    assert.equal(archived.status, 'completed');
    assert.ok(archived.archived_at, 'P5 archive stamps terminal Dream without changing status');
    const visible = await drain(client.beta.dreams.list({ betas: MANAGED_BETAS }));
    assert.ok(!visible.some((dream) => dream.id === created.id), 'P5 default list hides archived');
    const withArchived = await drain(client.beta.dreams.list({
      include_archived: true,
      betas: MANAGED_BETAS,
    }));
    assert.ok(withArchived.some((dream) => dream.id === created.id), 'P5 include_archived restores it');
    pass('P5 official SDK list, terminal guard and archive semantics agree');

    pass('P6 official TypeScript SDK crossed Session, Memory, Dream and Files modules end to end');
  });
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
