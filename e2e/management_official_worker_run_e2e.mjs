// EnvironmentWorker differential, execution plane: drive a self-hosted session's
// tool calls with the OFFICIAL SDK SessionToolRunner (client.beta.sessions.events
// .toolRunner) — the per-session half of EnvironmentWorker — instead of hand-rolled
// user.custom_tool_result posts. Composed with the work-queue claim/stop this is the
// full official worker per-item flow: claim → run the session's tools while the
// runner streams events → stop. If awaken's session event + tool-result wire matches
// what the official runner expects, a betaZodTool answering `submit_answer` drives the
// session to completion unmodified.
//
// Run: (from e2e/)  node management_official_worker_run_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { betaZodTool } from '@anthropic-ai/sdk/helpers/beta/zod';
import * as z from 'zod';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38293);

// The worker's implementation of the session's client-executed `submit_answer`
// tool — the official runner parses the tool_use input against this schema and
// dispatches run(); its return is posted back as the tool result.
const submitAnswer = betaZodTool({
  name: 'submit_answer',
  description: 'Answer the question the agent asks.',
  inputSchema: z.object({ question: z.string() }),
  run: () => '42',
});

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  const { server, baseUrl } = spawnServer('worker', PORT);
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const env = await client.beta.environments.create({
      name: 'official-worker-run-env',
      config: { type: 'self_hosted' },
      betas: BETAS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: env.id,
      betas: BETAS,
    });

    // Claim the session work item (the worker's control plane).
    let sessionWork = null;
    for (let i = 0; i < 5 && !sessionWork; i += 1) {
      const work = await client.beta.environments.work.poll(env.id, { betas: BETAS });
      if (!work) break;
      if (work.data.type === 'session') sessionWork = work;
      else await client.beta.environments.work.stop(work.id, { environment_id: env.id, betas: BETAS });
    }
    assert.ok(sessionWork, 'claimed the session work item');
    await client.beta.environments.work.ack(sessionWork.id, { environment_id: env.id, betas: BETAS });

    // Drive the session turn the worker is responsible for.
    await client.beta.sessions.events.send(session.id, {
      betas: BETAS,
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'answer it' }] }],
    });

    // Run the session's tool calls with the OFFICIAL SessionToolRunner + our tool.
    // `maxIdleMs` ends iteration ~2s after end_turn (default is 60s); the abort
    // signal is a hard hang-guard so a wire mismatch surfaces as a failure, not a hang.
    const dispatched = [];
    for await (const call of client.beta.sessions.events.toolRunner(session.id, {
      tools: [submitAnswer],
      betas: BETAS,
      maxIdleMs: 2000,
      signal: AbortSignal.timeout(20000),
    })) {
      dispatched.push(call.name ?? call.toolUse?.name);
    }
    assert.ok(
      dispatched.includes('submit_answer'),
      `the official SessionToolRunner dispatched submit_answer (saw: ${dispatched.join(',') || '∅'})`,
    );
    pass('the official SessionToolRunner ran the session’s tool call against awaken');

    // The session completed: the model replied with the tool result.
    const events = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
    const idle = events.reverse().find((e) => e.type === 'session.status_idle');
    assert.equal(idle?.stop_reason?.type, 'end_turn', 'the session completed with end_turn after the tool ran');
    pass('the session reached end_turn — the official worker execution plane drove it to completion');

    // Force-stop the work item (worker exit).
    const stopped = await client.beta.environments.work.stop(sessionWork.id, {
      environment_id: env.id,
      force: true,
      betas: BETAS,
    });
    assert.notEqual(stopped.state, 'active', 'the worker force-stops the item on exit');
    pass('worker force-stops the item on exit');

    console.log('E2E PASS: the official SessionToolRunner drives an awaken session to completion.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
  }
}

main();
