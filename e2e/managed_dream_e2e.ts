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
// | P0 | official SDK default Dream beta | create | accepted typed Dream |
// | P1 | source store + one completed Session | execute | completed with output/session refs |
// | P2 | committed transcript text | prepare/cleanup | private InlineBytes create no public transcript File |
// | P3 | prepared result | complete | source unchanged; output has a distinct id and cloned content |
// | P4 | auxiliary execution | terminal | ordinary Session is terminated and marked Dream origin |
// | P5 | prepared Running Dream | cancel/retrieve/repeat | canceled is immediate, durable, idempotent; public Files remain unchanged |
// | P6 | terminal Dream | list/archive/cancel | filters/archive work; completed cancel is 400 |
// | P7 | all above through one process boundary | SDK decode | no compatibility shim or raw Dream request |
//
// Causes: official SDK input shape and beta injection; source MemoryStore and
// committed Session state; Dream lifecycle state; terminal mutations.
// Constraints: one source store, one non-running Session, an independently
// addressable output store, and no raw HTTP request for a Dream operation.
// Effects: typed asynchronous creation, private frozen JSONL, an ordinary
// auxiliary Session, isolated output content, and correct list/cancel/archive
// projections.
// Decision rules: P0-P7 above jointly cover the successful cross-module path
// and every terminal operation exposed by the official Dreams SDK resource.
//
// Run from e2e/: npm run test:dream

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { existsSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AnthropicBeta } from '@anthropic-ai/sdk/resources/beta/beta';
import type { BetaDream } from '@anthropic-ai/sdk/resources/beta/dreams';
import type { BetaManagedAgentsMemory } from '@anthropic-ai/sdk/resources/beta/memory-stores/memories';
// @ts-ignore -- the shared harness intentionally remains plain JavaScript for
// the repository's Node and TypeScript matrices.
import { pass, withServer, waitForValue, waitForSessionEventReceipt, cleanupFixtureTree } from './harness.mjs';

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
  // S1 create admits one initial User Event; S2 its own processed_at and later
  // reply/idle commit. S1&&!S2 => observe only; S1+S2 => export this transcript.
  // A pre-activation idle snapshot cannot satisfy this source preparation.
  const initial = await waitForValue(
    async () => (await drain(client.beta.sessions.events.list(sessionID, { betas: MANAGED_BETAS })))
      .find((event) => event.type === 'user.message'),
    Boolean,
    'Dream source initial User Event identity',
  );
  await waitForSessionEventReceipt(
    client, sessionID, initial.id, MANAGED_BETAS,
    ({ delta }: { delta: { type: string }[] }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    'Dream source exact initial receipt to settle',
  );
}

async function waitForDream(client: Anthropic, dreamID: string): Promise<BetaDream> {
  return waitForValue(
    () => client.beta.dreams.retrieve(dreamID),
    (dream: BetaDream) => ['completed', 'failed', 'canceled'].includes(dream.status),
    `Dream ${dreamID} committed terminal state`,
  );
}

async function waitForPreparedDream(client: Anthropic, dreamID: string): Promise<BetaDream> {
  return waitForValue(
    async () => {
      const dream = await client.beta.dreams.retrieve(dreamID);
      assert.ok(!['completed', 'failed', 'canceled'].includes(dream.status), 'held Dream stays nonterminal until cancellation');
      return dream;
    },
    (dream: BetaDream) => dream.status === 'running' && Boolean(dream.session_id) && dream.outputs.length === 1,
    `Dream ${dreamID} committed prepared Running state`,
  );
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
  // The operation-derived owner graph never selects this scenario for a client
  // without Dreams. A direct unsupported invocation fails before any process,
  // instead of using raw HTTP to certify a different SDK's resource surface.
  assert.equal(typeof new Anthropic({ apiKey: 'e2e-dummy' }).beta.dreams?.create, 'function', 'official SDK Dreams surface is required');
  const barrierDirectory = mkdtempSync(join(tmpdir(), 'awaken-dream-pre-execute-'));
  try {
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

    const beforeInvalid = await drain(client.beta.dreams.list());
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
      })), 400, `P0b unsupported/malformed model reference fails admission: ${model}`);
    }
    const afterInvalid = await drain(client.beta.dreams.list());
    assert.equal(afterInvalid.length, beforeInvalid.length, 'P0b invalid references persist no Dream');
    pass('P0b malformed qualifiers and outbound A2A Dream references fail before persistence');

    // P5 cause/effect graph: C1 the real executor has completed preparation
    // but its test-only decorator has not called execute; C2=prepare commits the
    // output/session coordinates and private InlineBytes; C3=the public Files
    // catalog is observed before/after prepare; C4=the official SDK cancels Running.
    // Effects: E1=no public File is created; E2=cancel
    // returns one typed terminal; E3=retrieve and repeated cancel observe that
    // durable terminal without running cleanup twice. The deterministic Rust
    // state-machine tests complement this process proof with every cancel/worker
    // completion interleaving.
    const filesBeforePreparation = await drain(client.beta.files.list());
    const cancellationTarget = await client.beta.dreams.create({
      inputs: [
        { type: 'memory_store', memory_store_id: sourceStore.id },
        { type: 'sessions', session_ids: [sourceSession.id] },
      ],
      model: 'claude-sonnet-5',
      instructions: '[awaken-test:pause-dream-before-execution]',
    });
    const preparedCancellationTarget = await waitForPreparedDream(client, cancellationTarget.id);
    assert.deepEqual(
      await drain(client.beta.files.list()), filesBeforePreparation,
      'P5 Running preparation mounts private JSONL without publishing a File',
    );
    await waitForValue(
      async () => existsSync(join(barrierDirectory, `${cancellationTarget.id}.prepared`)),
      Boolean, 'P5 exact prepared execution barrier entered',
    );
    const canceled = await client.beta.dreams.cancel(cancellationTarget.id);
    assert.equal(canceled.status, 'canceled', 'P5 official cancel succeeds before completion');
    assert.equal(canceled.session_id, preparedCancellationTarget.session_id, 'P5 prepared identity remains stable');
    assert.equal(canceled.outputs.length, 1, 'P5 prepared output remains auditable after cancellation');
    assert.ok(canceled.ended_at, 'P5 cancellation publishes one terminal timestamp');
    assert.equal(
      (await client.beta.dreams.retrieve(cancellationTarget.id)).status,
      'canceled',
      'P5 cancellation is durably observable',
    );
    const repeatedCancellation = await client.beta.dreams.cancel(cancellationTarget.id);
    assert.equal(repeatedCancellation.status, 'canceled', 'P5 repeated cancel is idempotent');
    assert.equal(repeatedCancellation.ended_at, canceled.ended_at, 'P5 terminal identity is stable');
    assert.deepEqual(
      await drain(client.beta.files.list()), filesBeforePreparation,
      'P5 cancellation leaves the public File catalog unchanged',
    );
    const canceledPage = await drain(client.beta.dreams.list({ statuses: ['canceled'] }));
    assert.ok(
      canceledPage.some((dream) => dream.id === cancellationTarget.id),
      'P5 canceled filter includes the durable terminal',
    );
    await client.beta.dreams.archive(cancellationTarget.id);
    pass('P5 private transcripts remain unlisted and cancellation is durable and idempotent');

    const created = await client.beta.dreams.create({
      inputs: [
        { type: 'memory_store', memory_store_id: sourceStore.id },
        { type: 'sessions', session_ids: [sourceSession.id] },
      ],
      model: 'claude-sonnet-5',
      instructions: 'Retain verified project conventions.',
    });
    assert.equal(created.type, 'dream', 'P0 official SDK decodes the Dream resource');
    assert.equal(created.status, 'pending', 'P0 create is asynchronous');
    pass('P0 official SDK default Dream beta creates an asynchronous Dream');

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

    const files = await drain(client.beta.files.list());
    const transcript = files.find((file) => file.filename === `${sourceSession.id}.jsonl`);
    assert.equal(transcript, undefined, 'P2 private JSONL never becomes a public File');
    assert.deepEqual(files, filesBeforePreparation, 'P2 completed Dream leaves the File catalog unchanged');
    pass('P2 frozen Dream transcripts remain private InlineBytes');

    const completed = await drain(client.beta.dreams.list({ statuses: ['completed'] }));
    assert.ok(completed.some((dream) => dream.id === created.id), 'P6 status filter includes Dream');
    assert.equal(
      await status(() => client.beta.dreams.cancel(created.id)),
      400,
      'P6 completed Dream cannot be canceled',
    );
    const archived = await client.beta.dreams.archive(created.id);
    assert.equal(archived.status, 'completed');
    assert.ok(archived.archived_at, 'P6 archive stamps terminal Dream without changing status');
    const visible = await drain(client.beta.dreams.list());
    assert.ok(!visible.some((dream) => dream.id === created.id), 'P6 default list hides archived');
    const withArchived = await drain(client.beta.dreams.list({ include_archived: true }));
    assert.ok(withArchived.some((dream) => dream.id === created.id), 'P6 include_archived restores it');
    pass('P6 official SDK list, terminal guard and archive semantics agree');

    pass('P7 official TypeScript SDK crossed Session, Memory, Dream and Files modules end to end');
    }, { AWAKEN_TEST_DREAM_PRE_EXECUTE_BARRIER_DIR: barrierDirectory });
  } finally {
    cleanupFixtureTree(barrierDirectory);
  }
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
