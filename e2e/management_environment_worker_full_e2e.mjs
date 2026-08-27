// Full official TypeScript EnvironmentWorker/SessionToolRunner compatibility.
//
// Cause/effect graph:
//   Session work -> WorkPoller poll/ack -> setupSkills -> SessionToolRunner
//   stream/tool/result || heartbeat -> cleanup -> force-stop -> next poll.
// Decision table:
//   W1 run(explicit)       -> complete turn, heartbeat, cleanup, stopped work
//   W2 handleItem(explicit)-> no second claim; same per-item effects
//   W3 handleItem(env)     -> ANTHROPIC_* fallback has the same effects
//   W4 missing field       -> AnthropicError before any network side effect
//   W5 pinned Skill        -> exact version download and cleanup
//   W6 abort               -> in-flight helper unwinds and force-stops
//   W7 stop failure        -> completed helper result survives; lease stays retryable
//   W8 memory + secret     -> download, local edit, final sync, directory cleanup

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import * as z from 'zod';
import {
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { requireOrSkipBwrap } from './bwrap_capability.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38341);

async function drain(page) {
  const rows = [];
  for await (const row of page) rows.push(row);
  return rows;
}

function instrumentedTool(counters) {
  return betaZodTool({
    name: 'submit_answer',
    description: 'Return the deterministic worker answer.',
    inputSchema: z.object({ question: z.string() }),
    run: async () => {
      counters.runs += 1;
      await new Promise((resolveDelay) => setTimeout(resolveDelay, 80));
      return '42';
    },
    close: async () => {
      counters.closes += 1;
    },
  });
}

async function createClaimableSession(client, name, agent = 'assistant', resources = undefined) {
  const environment = await client.beta.environments.create({
    name: `${name}-environment`,
    config: { type: 'self_hosted' },
    betas: BETAS,
  });
  // The full worker serves Session work. Drain the separately modelled
  // healthcheck first so this fixture isolates the Session helper path.
  const healthcheck = await client.beta.environments.work.poll(environment.id, { betas: BETAS });
  assert.equal(healthcheck?.data.type, 'healthcheck');
  await client.beta.environments.work.stop(healthcheck.id, {
    environment_id: environment.id,
    force: true,
    betas: BETAS,
  });
  const session = await client.beta.sessions.create({
    agent,
    environment_id: environment.id,
    ...(resources === undefined ? {} : { resources }),
    betas: BETAS,
  });
  return { environment, session };
}

async function sendTask(client, sessionID) {
  // Worker-consumption precondition. C1=the Event batch is durably accepted;
  // C2=its asynchronous Run has committed one answerable custom tool and the
  // matching requires_action boundary. E1=the official Worker starts from a
  // stable replayable input, independent of poll/setup timing. Decision rules:
  // P1=C1&&!C2=>observe only; P2=C1+C2=>return the exact receipt. Observation
  // cannot execute or answer the tool, so SessionToolRunner remains sole owner.
  const receipt = await client.beta.sessions.events.send(sessionID, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: 'answer it' }],
    }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'P1 exact EnvironmentWorker task receipt');
  await waitForSessionEventReceipt(
    client,
    sessionID,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.custom_tool_use')
      && [...delta].reverse().find((event) => event.type === 'session.status_idle')
        ?.stop_reason?.type === 'requires_action',
    'P1 custom tool task commits before the official Worker consumes it',
  );
  return receiptId;
}

async function assertExactlyOneResult(client, sessionID, receiptId, counters) {
  // Result rule R0: C1=exact task receipt; C2=official worker result; C3=end_turn.
  // E1=one correlated result. Constraint: tool execution stays SDK-owned.
  // C1+C2&&!C3=>observe; C1+C2+C3=>E1.
  const { events } = await waitForSessionEventReceipt(
    client,
    sessionID,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'user.custom_tool_result')
      && delta.some((event) => event.type === 'session.status_idle'
        && event.stop_reason?.type === 'end_turn'),
    'official EnvironmentWorker result reaches end_turn',
  );
  const uses = events.filter((event) => event.type === 'agent.custom_tool_use');
  const results = events.filter((event) => event.type === 'user.custom_tool_result');
  assert.equal(counters.runs, 1, 'the official runner invokes the tool exactly once');
  assert.equal(results.length, 1, `one durable result event: ${events.map((event) => event.type)}`);
  assert.equal(results[0].custom_tool_use_id, uses[0].id, 'result answers the exact tool-use id');
  assert.ok(events.some((event) =>
    event.type === 'session.status_idle' && event.stop_reason?.type === 'end_turn'));
}

async function main() {
  const workdir = mkdtempSync(resolve(tmpdir(), 'awaken-environment-worker-'));
  const { server, baseUrl } = spawnServer('worker', PORT);
  const savedEnv = new Map();
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // W1: the actual high-level run loop owns poll, ack, heartbeat and stop.
    {
      const counters = { runs: 0, closes: 0 };
      const { environment, session } = await createClaimableSession(client, 'worker-run');
      const receiptId = await sendTask(client, session.id);
      const controller = new AbortController();
      const worker = client.beta.environments.work.worker({
        environmentId: environment.id,
        environmentKey: 'e2e-env-key',
        workdir,
        tools: [instrumentedTool(counters)],
        maxIdleMs: 100,
        signal: controller.signal,
      });
      const running = worker.run();
      const stopped = await waitForValue(
        async () => {
          const rows = await drain(client.beta.environments.work.list(environment.id, { betas: BETAS }));
          return rows.find((row) => row.data.type === 'session' && row.state === 'stopped');
        },
        (value) => value !== undefined,
        'EnvironmentWorker.run did not force-stop the Session work',
        { timeoutMs: 20_000, pollMs: 40 },
      );
      controller.abort();
      await running;
      assert.ok(stopped.acknowledged_at, 'WorkPoller acknowledged the item');
      assert.ok(stopped.latest_heartbeat_at, 'heartbeat ran concurrently with SessionToolRunner');
      assert.equal(counters.closes, 1, 'SessionToolRunner closes every tool');
      await assertExactlyOneResult(client, session.id, receiptId, counters);
      pass('W1 EnvironmentWorker.run: poll -> ack -> tool/heartbeat -> cleanup -> force-stop');
    }

    // W2: explicit handleItem starts after an external owner has claimed/acked.
    {
      const counters = { runs: 0, closes: 0 };
      const { environment, session } = await createClaimableSession(client, 'worker-handle-explicit');
      const environmentClient = new Anthropic({ authToken: 'e2e-env-key', baseURL: baseUrl }); // awaken-allow: secret
      const work = await environmentClient.beta.environments.work.poll(environment.id, { betas: BETAS });
      assert.equal(work?.data.id, session.id);
      await environmentClient.beta.environments.work.ack(work.id, {
        environment_id: environment.id,
        betas: BETAS,
      });
      const receiptId = await sendTask(client, session.id);
      const worker = client.beta.environments.work.worker({
        workdir,
        tools: [instrumentedTool(counters)],
        maxIdleMs: 100,
      });
      await worker.handleItem({
        workId: work.id,
        environmentId: environment.id,
        sessionId: session.id,
        environmentKey: 'e2e-env-key',
      });
      await assertExactlyOneResult(client, session.id, receiptId, counters);
      assert.equal(
        (await client.beta.environments.work.retrieve(work.id, {
          environment_id: environment.id,
          betas: BETAS,
        })).state,
        'stopped',
      );
      pass('W2 EnvironmentWorker.handleItem accepts explicit claimed-work coordinates');
    }

    // W3: ant worker poll --on-work passes the same coordinates through env.
    {
      const counters = { runs: 0, closes: 0 };
      const { environment, session } = await createClaimableSession(client, 'worker-handle-env');
      const environmentClient = new Anthropic({ authToken: 'e2e-env-key', baseURL: baseUrl }); // awaken-allow: secret
      const work = await environmentClient.beta.environments.work.poll(environment.id, { betas: BETAS });
      await environmentClient.beta.environments.work.ack(work.id, {
        environment_id: environment.id,
        betas: BETAS,
      });
      const receiptId = await sendTask(client, session.id);
      for (const [key, value] of Object.entries({
        ANTHROPIC_WORK_ID: work.id,
        ANTHROPIC_ENVIRONMENT_ID: environment.id,
        ANTHROPIC_SESSION_ID: session.id,
        ANTHROPIC_ENVIRONMENT_KEY: 'e2e-env-key',
      })) {
        savedEnv.set(key, process.env[key]);
        process.env[key] = value;
      }
      const worker = client.beta.environments.work.worker({
        workdir,
        tools: [instrumentedTool(counters)],
        maxIdleMs: 100,
      });
      await worker.handleItem();
      await assertExactlyOneResult(client, session.id, receiptId, counters);
      pass('W3 handleItem resolves all ANTHROPIC_* environment fallbacks');
      for (const key of savedEnv.keys()) delete process.env[key];
      for (const [key, value] of savedEnv) if (value !== undefined) process.env[key] = value;
      savedEnv.clear();
    }

    // W4: each missing coordinate fails locally, naming the first missing value.
    {
      const keys = [
        'ANTHROPIC_WORK_ID',
        'ANTHROPIC_ENVIRONMENT_ID',
        'ANTHROPIC_SESSION_ID',
        'ANTHROPIC_ENVIRONMENT_KEY',
      ];
      for (const key of keys) {
        savedEnv.set(key, process.env[key]);
        delete process.env[key];
      }
      const worker = client.beta.environments.work.worker({ workdir, tools: [] });
      await assert.rejects(() => worker.handleItem(), /workId is required/);
      await assert.rejects(
        () => worker.handleItem({ workId: 'work_test' }),
        /environmentId is required/,
      );
      await assert.rejects(
        () => worker.handleItem({ workId: 'work_test', environmentId: 'env_test' }),
        /sessionId is required/,
      );
      await assert.rejects(
        () => worker.handleItem({
          workId: 'work_test',
          environmentId: 'env_test',
          sessionId: 'sesn_test',
        }),
        /environmentKey is required/,
      );
      pass('W4 missing handleItem coordinates fail before network I/O');
    }

    // W5: setupSkills resolves a pinned numeric version, extracts it before the
    // tools factory runs, and removes the extracted directory after the item.
    {
      const management = spawnServer('management', PORT + 1);
      try {
        await waitForPort(PORT + 1, 900_000, management.server);
        const managementClient = new Anthropic({ apiKey: 'e2e-dummy', baseURL: management.baseUrl });
        const skillV1 = '---\nname: worker-greeter\ndescription: worker v1\n---\nPINNED_WORKER_SKILL_V1';
        const skillV2 = '---\nname: worker-greeter\ndescription: worker v2\n---\nLATEST_WORKER_SKILL_V2';
        const skill = await managementClient.beta.skills.create({
          files: [await toFile(Buffer.from(skillV1), 'SKILL.md')],
        });
        await managementClient.beta.skills.versions.create(skill.id, {
          files: [await toFile(Buffer.from(skillV2), 'SKILL.md')],
        });
        const agent = await managementClient.beta.agents.create({
          name: `worker-skill-${Date.now()}`,
          model: 'claude-sonnet-5',
          skills: [{ type: 'custom', skill_id: skill.id, version: '1' }],
          betas: BETAS,
        });
        const { environment, session } = await createClaimableSession(
          managementClient,
          'worker-skill-pinned',
          agent.id,
        );
        const environmentClient = new Anthropic({
          authToken: 'e2e-env-key', // awaken-allow: secret
          baseURL: management.baseUrl,
        });
        const work = await environmentClient.beta.environments.work.poll(environment.id, {
          betas: BETAS,
        });
        await environmentClient.beta.environments.work.ack(work.id, {
          environment_id: environment.id,
          betas: BETAS,
        });
        let downloaded;
        const worker = managementClient.beta.environments.work.worker({
          workdir,
          tools: () => {
            downloaded = readFileSync(join(workdir, 'skills', 'worker-greeter', 'SKILL.md'), 'utf8');
            return [];
          },
          maxIdleMs: 50,
        });
        await worker.handleItem({
          workId: work.id,
          environmentId: environment.id,
          sessionId: session.id,
          environmentKey: 'e2e-env-key',
          signal: AbortSignal.timeout(500),
        });
        assert.match(downloaded, /PINNED_WORKER_SKILL_V1/);
        assert.doesNotMatch(downloaded, /LATEST_WORKER_SKILL_V2/);
        assert.equal(
          existsSync(join(workdir, 'skills', 'worker-greeter')),
          false,
          'downloaded Skill directory is cleaned after the work item',
        );
        pass('W5 setupSkills downloads the pinned version and cleans it after execution');

        const failureCase = await createClaimableSession(
          managementClient,
          'worker-skill-cleanup-failure',
          agent.id,
        );
        const failureWork = await environmentClient.beta.environments.work.poll(
          failureCase.environment.id,
          { betas: BETAS },
        );
        await environmentClient.beta.environments.work.ack(failureWork.id, {
          environment_id: failureCase.environment.id,
          betas: BETAS,
        });
        const skillsRoot = join(workdir, 'skills');
        try {
          await managementClient.beta.environments.work.worker({
            workdir,
            tools: () => {
              chmodSync(skillsRoot, 0o500);
              return [];
            },
          }).handleItem({
            workId: failureWork.id,
            environmentId: failureCase.environment.id,
            sessionId: failureCase.session.id,
            environmentKey: 'e2e-env-key',
            signal: AbortSignal.timeout(500),
          });
          assert.equal((await managementClient.beta.environments.work.retrieve(failureWork.id, {
            environment_id: failureCase.environment.id,
            betas: BETAS,
          })).state, 'stopped', 'cleanup failure does not skip force-stop');
          assert.equal(
            existsSync(join(skillsRoot, 'worker-greeter')),
            true,
            'injected cleanup denial leaves evidence for the fixture owner',
          );
          pass('W5b Skill cleanup failure is non-masking and work is still force-stopped');
        } finally {
          chmodSync(skillsRoot, 0o700);
          rmSync(join(skillsRoot, 'worker-greeter'), { recursive: true, force: true });
        }
      } finally {
        await stopServer(management.server);
      }
    }

    // W6: external cancellation still executes the per-item finally block and
    // force-stops the lease even when no Session event ever arrives.
    {
      const { environment, session } = await createClaimableSession(client, 'worker-abort');
      const environmentClient = new Anthropic({ authToken: 'e2e-env-key', baseURL: baseUrl }); // awaken-allow: secret
      const work = await environmentClient.beta.environments.work.poll(environment.id, { betas: BETAS });
      await environmentClient.beta.environments.work.ack(work.id, {
        environment_id: environment.id,
        betas: BETAS,
      });
      const controller = new AbortController();
      const worker = client.beta.environments.work.worker({ workdir, tools: [], maxIdleMs: 0 });
      const handling = worker.handleItem({
        workId: work.id,
        environmentId: environment.id,
        sessionId: session.id,
        environmentKey: 'e2e-env-key',
        signal: controller.signal,
      });
      setTimeout(() => controller.abort(), 80);
      await handling;
      assert.equal((await client.beta.environments.work.retrieve(work.id, {
        environment_id: environment.id,
        betas: BETAS,
      })).state, 'stopped');
      pass('W6 abort unwinds the runner and force-stops the claimed work item');
    }

    // W7: force-stop transport failure is logged by the official helper and
    // does not replace the completed runner result. The authority remains
    // active until an independent owner performs the cleanup.
    {
      const { environment, session } = await createClaimableSession(client, 'worker-stop-failure');
      const environmentClient = new Anthropic({ authToken: 'e2e-env-key', baseURL: baseUrl }); // awaken-allow: secret
      const work = await environmentClient.beta.environments.work.poll(environment.id, { betas: BETAS });
      await environmentClient.beta.environments.work.ack(work.id, {
        environment_id: environment.id,
        betas: BETAS,
      });
      const stopFailing = new Anthropic({
        apiKey: 'e2e-dummy',
        baseURL: baseUrl,
        fetch: async (input, init) => {
          const url = typeof input === 'string' ? input : input.url;
          if (url.endsWith(`/work/${work.id}/stop?beta=true`)) {
            return new Response(JSON.stringify({
              type: 'error',
              error: { type: 'api_error', message: 'injected stop failure' },
            }), { status: 503, headers: { 'content-type': 'application/json' } });
          }
          return fetch(input, init);
        },
      });
      await stopFailing.beta.environments.work.worker({
        workdir,
        tools: [],
        maxIdleMs: 50,
      }).handleItem({
        workId: work.id,
        environmentId: environment.id,
        sessionId: session.id,
        environmentKey: 'e2e-env-key',
        signal: AbortSignal.timeout(500),
      });
      assert.equal((await client.beta.environments.work.retrieve(work.id, {
        environment_id: environment.id,
        betas: BETAS,
      })).state, 'active');
      await environmentClient.beta.environments.work.stop(work.id, {
        environment_id: environment.id,
        force: true,
        betas: BETAS,
      });
      pass('W7 force-stop failure is non-masking and leaves authority available for retry');
    }

    // W8 cause/effect rule: C1=attached read-write MemoryStore; C2=claimed
    // Work carries the SDK-defined secret; C3=remote seed; C4=local edit.
    // C1&&!C2 => official helper refuses an amnesiac mount (SDK-owned negative
    // test); C1+C2+C3 => E1 download before tools factory; +C4 => E2 final CAS
    // sync and E3 owned mount cleanup. This rule proves our WorkSecret and
    // Session projection drive the unmodified 0.120 helper end to end.
    {
      const management = spawnServer('management', PORT + 2);
      try {
        await waitForPort(PORT + 2, 900_000, management.server);
        const managementClient = new Anthropic({
          apiKey: 'e2e-dummy',
          baseURL: management.baseUrl,
        });
        const store = await managementClient.beta.memoryStores.create({
          name: `worker-memory-${Date.now()}`,
          betas: ['agent-memory-2026-07-22'],
        });
        const seeded = await managementClient.beta.memoryStores.memories.create(store.id, {
          path: '/notes.md',
          content: 'REMOTE_MEMORY_SEED',
          betas: ['agent-memory-2026-07-22'],
        });
        const { environment, session } = await createClaimableSession(
          managementClient,
          'worker-memory-secret',
          'assistant',
          [{ type: 'memory_store', memory_store_id: store.id, access: 'read_write' }],
        );
        const environmentClient = new Anthropic({
          authToken: 'e2e-env-key', // awaken-allow: secret
          baseURL: management.baseUrl,
        });
        const work = await environmentClient.beta.environments.work.poll(environment.id, {
          betas: BETAS,
        });
        assert.equal(work?.data.id, session.id);
        assert.equal(typeof work.secret, 'string', 'poll projection carries BetaWorkSecret');
        await environmentClient.beta.environments.work.ack(work.id, {
          environment_id: environment.id,
          betas: BETAS,
        });

        let mountedRoot;
        await managementClient.beta.environments.work.worker({
          workdir,
          tools: (ctx) => {
            assert.equal(ctx.allowedRoots?.length, 1, 'one attached store exposes one root');
            [mountedRoot] = ctx.allowedRoots;
            assert.equal(
              readFileSync(join(mountedRoot, 'notes.md'), 'utf8'),
              'REMOTE_MEMORY_SEED',
              'remote memory is present before tool construction',
            );
            writeFileSync(join(mountedRoot, 'notes.md'), 'UPDATED_BY_OFFICIAL_WORKER');
            return [];
          },
          maxIdleMs: 50,
          memorySyncIntervalMs: 5_000,
        }).handleItem({
          workId: work.id,
          environmentId: environment.id,
          sessionId: session.id,
          environmentKey: 'e2e-env-key',
          workSecret: work.secret,
          signal: AbortSignal.timeout(500),
        });

        const synced = await managementClient.beta.memoryStores.memories.retrieve(seeded.id, {
          memory_store_id: store.id,
          view: 'full',
          betas: ['agent-memory-2026-07-22'],
        });
        assert.equal(synced.content, 'UPDATED_BY_OFFICIAL_WORKER');
        assert.equal(existsSync(mountedRoot), false, 'SDK-owned memory mount is disposed');
        assert.equal((await managementClient.beta.environments.work.retrieve(work.id, {
          environment_id: environment.id,
          betas: BETAS,
        })).state, 'stopped');
        pass('W8 WorkSecret drives official MemoryStore download, sync, and cleanup');
      } finally {
        await stopServer(management.server);
      }
    }

    assert.deepEqual(
      readdirSync(join(workdir, 'skills')),
      [],
      'worker cleanup leaves no downloaded Skill directories',
    );
    console.log('E2E PASS: full official TypeScript EnvironmentWorker and SessionToolRunner lifecycle.');
  } finally {
    for (const [key, value] of savedEnv) {
      if (value === undefined) delete process.env[key];
      else process.env[key] = value;
    }
    await stopServer(server);
    rmSync(workdir, { recursive: true, force: true });
  }
}

// The 0.120 response contract deliberately returns sandbox-absolute MemoryStore
// paths under /mnt/memory. Re-exec this fixture in the same bubblewrap shape as
// Awaken's Namespace sandbox so the test proves that exact path instead of
// weakening the projection for an unprivileged host checkout.
if (process.env.AWAKEN_WORKER_MANAGED_MOUNT_NAMESPACE === '1') {
  main();
} else {
  if (!requireOrSkipBwrap()) {
    process.exit(0);
  }
  const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const targetDir = join(repositoryRoot, 'target');
  const result = spawnSync('bwrap', [
    '--unshare-user',
    '--uid', '0',
    '--gid', '0',
    '--ro-bind', '/', '/',
    '--bind', targetDir, targetDir,
    '--dev-bind', '/dev', '/dev',
    '--proc', '/proc',
    '--tmpfs', '/tmp',
    '--tmpfs', '/mnt',
    '--dir', '/mnt/memory',
    '--', process.execPath, fileURLToPath(import.meta.url),
  ], {
    cwd: process.cwd(),
    env: { ...process.env, AWAKEN_WORKER_MANAGED_MOUNT_NAMESPACE: '1' },
    stdio: 'inherit',
  });
  if (result.error) throw result.error;
  if (result.signal) throw new Error(`managed worker fixture terminated by ${result.signal}`);
  process.exitCode = result.status ?? 1;
}
