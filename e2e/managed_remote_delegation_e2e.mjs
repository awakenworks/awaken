// Remote A2A delegation end-to-end via the Anthropic TS SDK.
//
// Server A (delegate-remote) delegates `agent_run` for `researcher` to a REMOTE A2A
// agent — server B — instead of a local sub-run. A's resolver sends the delegate
// input to B over A2A (message:send → poll get_task), B (echo) processes it as an
// A2A task and echoes, and the result flows back into A's turn. This exercises the
// remote-delegation path across a real A2A hop between two processes.
//
// Run: (from e2e/)  node managed_remote_delegation_e2e.mjs

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT_A = Number(process.env.E2E_PORT ?? 38184);
const PORT_B = PORT_A + 1;
const BASE_A = `http://127.0.0.1:${PORT_A}`;
const BETAS = ['managed-agents-2026-04-01'];
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE_A });

const listEvents = async (sessionId) => {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
};

async function main() {
  // Two real upstreams: B runs the echo peer, A runs the delegating coordinator —
  // each server's model crosses the real provider wire.
  const upstreamB = await startUpstream('echo');
  const upstreamA = await startUpstream('delegating');
  // Server B: the remote A2A agent (echo). Every server mounts the A2A adapter.
  const b = spawnServer('real', PORT_B, { ...realServerEnv('echo', upstreamB) });
  await waitForPort(PORT_B, 600_000, b.server);
  const publishedCard = await fetch(`http://127.0.0.1:${PORT_B}/v1/a2a/agent-card`).then(
    async (response) => {
      assert.equal(response.status, 200, 'remote Agent Card is discoverable before publication');
      return response.json();
    },
  );
  const securityFingerprint = `sha256:${createHash('sha256')
    .update(JSON.stringify([publishedCard.securitySchemes ?? {}, publishedCard.security ?? []]))
    .digest('hex')}`;
  // Server A: delegates `researcher` to B over A2A.
  const a = spawnServer('delegate-remote', PORT_A, {
    AWAKEN_REMOTE_AGENT_URL: `http://127.0.0.1:${PORT_B}`,
    AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT: securityFingerprint,
    ...realServerEnv('delegating', upstreamA, { mode: 'delegate-remote' }),
  });
  await waitForPort(PORT_A, 600_000, a.server);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'please delegate the research' }] }],
      betas: BETAS,
    });
    const events = await listEvents(session.id);

    // A delegated via agent_run.
    const toolUse = events.find((e) => e.type === 'agent.tool_use');
    assert.ok(
      toolUse && toolUse.name === 'agent_run',
      `A delegated via agent_run; events=${events.map((e) => e.type).join(',')}; messages=${JSON.stringify(events.filter((e) => e.type === 'agent.message').map((e) => e.content))}`,
    );
    pass('server A issued an agent_run delegation');

    // The remote peer (B) handled the delegated turn and echoed; the result flowed
    // back into A's reply across the A2A hop.
    const reply = JSON.stringify(events.filter((e) => e.type === 'agent.message').map((m) => m.content));
    assert.ok(reply.includes('delegate said:'), 'the delegate result flowed back into A');
    assert.ok(
      reply.includes('do the research'),
      `the remote peer echoed the delegated input back over A2A: ${reply}`,
    );
    pass('remote A2A delegation round-tripped: A → B (echo) → A');

    console.log('E2E PASS: pinned remote A2A delegation across two servers.');
  } finally {
    await stopServer(a.server);
    await stopServer(b.server);
    upstreamA.close();
    upstreamB.close();
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
