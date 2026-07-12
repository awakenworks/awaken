// Multi-protocol concurrency + isolation e2e: ONE `awaken-server` process,
// FOUR wire frontdoors, ~40 requests IN FLIGHT AT ONCE spread across all four,
// interleaved in a single `Promise.all`. Every request carries a UNIQUE sentinel
// marker in its input; with the `echo` model each reply is `Echo: <input>`, so a
// request's own marker must appear verbatim in ITS reply and NO sibling's marker
// may bleed in. Proves the shared process fans concurrent load across protocols
// without dropping requests or crossing responses.
//
//   - managed : @anthropic-ai/sdk  beta.sessions.* over /v1/sessions
//   - ai-sdk  : POST /v1/ai-sdk/chat, read the SSE UI message stream to [DONE]
//   - ag-ui   : @ag-ui/client HttpAgent at /v1/ag-ui/agents/assistant
//   - a2a     : @a2a-js/sdk A2AClient.fromCardUrl(/v1/a2a/agent-card) + message:send
//
// Assertions: completeness (all ~40 resolve), isolation (own marker present, no
// sibling marker present), all four protocols concurrent (interleaved batch).
// Run: (from e2e/)  node multiprotocol_concurrency_e2e.mjs
//   PER_PROTOCOL=10 controls the fan-out per protocol (default 10 -> 40 total).

import assert from 'node:assert/strict';
import { randomBytes } from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { HttpAgent } from '@ag-ui/client';
import { A2AClient } from '@a2a-js/sdk/client';
import { withRealServer, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38433);
const PER_PROTOCOL = Number(process.env.PER_PROTOCOL ?? 10);
const BETAS = ['managed-agents-2026-04-01'];
const PROTOCOLS = ['managed', 'aisdk', 'agui', 'a2a'];

// --- one request per protocol; each returns the reply text for `marker` --------

// managed: fresh session, one user turn carrying `marker`, read the agent.message.
async function reqManaged(ctx, marker) {
  const session = await ctx.anthropic.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  await ctx.anthropic.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const ev of ctx.anthropic.beta.sessions.events.list(session.id, { betas: BETAS })) {
    events.push(ev);
  }
  const message = events.find((e) => e.type === 'agent.message');
  assert.ok(message, `managed produced an agent.message (${events.map((e) => e.type)})`);
  return message.content?.[0]?.text ?? '';
}

// ai-sdk: POST a UI message, drain the SSE stream to [DONE], concat text-delta.
async function reqAiSdk(ctx, marker) {
  const res = await fetch(`${ctx.base}/v1/ai-sdk/chat`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      threadId: `mpc-${marker}`,
      messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: marker }] }],
    }),
  });
  assert.equal(res.status, 200, `ai-sdk stream accepted for ${marker}`);
  const raw = await res.text();
  const frames = raw
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.startsWith('data: '))
    .map((l) => l.slice('data: '.length));
  assert.ok(frames.includes('[DONE]'), `ai-sdk stream closed with [DONE] for ${marker}`);
  return frames
    .filter((d) => d !== '[DONE]')
    .map((d) => JSON.parse(d))
    .filter((f) => f.type === 'text-delta')
    .map((f) => f.delta)
    .join('');
}

// ag-ui: fresh HttpAgent, run, read the last assistant message.
async function reqAgUi(ctx, marker) {
  const agent = new HttpAgent({ url: `${ctx.base}/v1/ag-ui/agents/assistant` });
  agent.messages = [{ id: 'u1', role: 'user', content: marker }];
  const run = await agent.runAgent();
  const produced = run?.newMessages ?? [];
  const last = produced[produced.length - 1];
  assert.ok(last && last.role === 'assistant', `ag-ui returned an assistant message for ${marker}`);
  return typeof last.content === 'string'
    ? last.content
    : (last.content ?? []).map((c) => c.text ?? '').join('');
}

// a2a: message:send on the shared client, read the Task's status message text.
async function reqA2a(ctx, marker) {
  const res = await ctx.a2a.sendMessage({
    message: {
      messageId: marker,
      contextId: `mpc-${marker}`,
      role: 'user',
      kind: 'message',
      parts: [{ kind: 'text', text: marker }],
    },
  });
  assert.ok(res.result, `a2a message:send returned a result for ${marker}`);
  return (res.result?.status?.message?.parts ?? [])
    .filter((p) => p.kind === 'text')
    .map((p) => p.text)
    .join('');
}

const SENDERS = { managed: reqManaged, aisdk: reqAiSdk, agui: reqAgUi, a2a: reqA2a };

async function main() {
  try {
    await withRealServer('echo', PORT, async (base) => {
      const ctx = {
        base,
        anthropic: new Anthropic({ apiKey: 'e2e-dummy', baseURL: base }),
        a2a: await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`),
      };

      // Build the full interleaved batch: PER_PROTOCOL requests for each of the 4
      // doors, each with a globally-unique marker. Interleave by protocol so the
      // single Promise.all has all four in flight at the same time.
      const batch = [];
      for (let i = 0; i < PER_PROTOCOL; i++) {
        for (const protocol of PROTOCOLS) {
          const marker = `m-${protocol}-${i}-${randomBytes(4).toString('hex')}`;
          batch.push({ protocol, i, marker });
        }
      }
      const total = batch.length;
      pass(`launching ${total} concurrent requests (${PER_PROTOCOL} x ${PROTOCOLS.length} protocols) in one Promise.all`);

      const settled = await Promise.allSettled(
        batch.map(async (r) => ({ ...r, reply: await SENDERS[r.protocol](ctx, r.marker) })),
      );

      // Completeness: every request must have resolved.
      const failures = settled
        .map((s, idx) => ({ s, r: batch[idx] }))
        .filter(({ s }) => s.status !== 'fulfilled');
      assert.equal(
        failures.length,
        0,
        `all ${total} requests completed (${failures.length} failed: ${failures
          .map(({ r, s }) => `${r.marker}: ${s.reason?.message ?? s.reason}`)
          .join('; ')})`,
      );
      const results = settled.map((s) => s.value);

      // Each reply is `Echo: <marker>` — own marker present, exactly one marker.
      const allMarkers = results.map((r) => r.marker);
      let isolationViolations = 0;
      const perProtocolCompleted = Object.fromEntries(PROTOCOLS.map((p) => [p, 0]));
      for (const r of results) {
        perProtocolCompleted[r.protocol]++;
        // Own marker must be echoed back verbatim.
        if (!r.reply.includes(r.marker)) {
          isolationViolations++;
          console.error(`  MISSING own marker: ${r.protocol} ${r.marker} -> ${JSON.stringify(r.reply)}`);
          continue;
        }
        // No sibling's marker may appear in this reply.
        for (const other of allMarkers) {
          if (other !== r.marker && r.reply.includes(other)) {
            isolationViolations++;
            console.error(
              `  CROSS-REQUEST BLEED: ${r.protocol} ${r.marker} reply carried sibling ${other} -> ${JSON.stringify(r.reply)}`,
            );
          }
        }
      }

      assert.equal(isolationViolations, 0, `${isolationViolations} isolation violation(s) detected`);
      for (const p of PROTOCOLS) {
        assert.equal(perProtocolCompleted[p], PER_PROTOCOL, `${p} completed all ${PER_PROTOCOL} requests`);
      }

      console.log(
        `concurrency=${total} protocols=${PROTOCOLS.length} completed=${results.length} isolation_violations=${isolationViolations} ` +
          `[${PROTOCOLS.map((p) => `${p}=${perProtocolCompleted[p]}`).join(' ')}]`,
      );
      pass('multi-protocol concurrency + isolation');
    });

    console.log(
      'E2E PASS: one awaken-server process fanned ~40 concurrent requests across managed + ai-sdk + ag-ui + a2a with full isolation.',
    );
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
