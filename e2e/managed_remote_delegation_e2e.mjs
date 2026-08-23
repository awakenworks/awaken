// Remote A2A delegation end-to-end via the Anthropic TS SDK.
//
// Server A uses fixed `list_agents` -> `send_to_agent` coordination for a
// `researcher` whose frozen backend is REMOTE A2A server B. The send receipt ends
// A's coordinator Run; B's echo returns asynchronously on the child Thread.
//
// Cause/effect graph: fixed roster + pinned remote card -> list -> accepted send
// -> A2A message:send/poll -> child reply event -> one tool-free report Run. A
// receipt does not contain the remote payload, and an unpinned/unreachable peer
// fails before a fabricated child reply (covered by the A2A failure suites).
//
// | rule | roster | remote pin | send | effect |
// | R1 | researcher | valid | accepted | child cross-posts B's echo; report Run sends no second child |
// | R2 | researcher | invalid/unreachable | rejected/failed | no fabricated reply |
// Constraints/invariant: the frozen remote Agent Card and ordinary child
// Thread/Run own execution; the coordinator receipt is admission, not payload.
// Decision rules are R1/R2 above; their effects are asserted on both child and
// root projections.
//
// Run: (from e2e/)  node managed_remote_delegation_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  agentCardSecurityFingerprint,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

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

async function waitForRemoteSettlement(sessionId, receiptId) {
  const settled = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => {
      const received = delta.some((event) => (
        event.type === 'agent.thread_message_received'
        && JSON.stringify(event.content).includes('do the research')
      ));
      const childSettled = delta.some((event) => (
        event.type === 'session.thread_status_idle'
        && event.stop_reason?.type === 'end_turn'
      ));
      const reportSettled = delta.some((event) => (
        event.type === 'agent.message'
        && JSON.stringify(event.content).includes('coordination completed from child report')
      ));
      return received
        && childSettled
        && reportSettled
        && delta.at(-1)?.type === 'session.status_idle';
    },
    `Session ${sessionId} did not settle after remote child work`,
    { timeoutMs: 120_000, pollMs: 200 },
  );
  return settled.events;
}

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
  const securityFingerprint = agentCardSecurityFingerprint(publishedCard);
  // Server A: delegates `researcher` to B over A2A.
  const a = spawnServer('delegate-remote', PORT_A, {
    AWAKEN_REMOTE_AGENT_URL: `http://127.0.0.1:${PORT_B}`,
    AWAKEN_REMOTE_AGENT_SECURITY_FINGERPRINT: securityFingerprint,
    ...realServerEnv('delegating', upstreamA, { mode: 'delegate-remote' }),
  });
  await waitForPort(PORT_A, 600_000, a.server);
  try {
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    // C1=exact coordinator receipt; C2=remote child/report terminal. E1=C2
    // after C1. K: remote Card and child Thread own execution. Decision R1
    // C1&&!C2=>retry; R2 C1+C2=>assert both projections.
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'please delegate the research' }] }],
      betas: BETAS,
    });
    const receiptId = receipt.data[0]?.id;
    assert.equal(typeof receiptId, 'string', 'R1 exact remote-delegation User receipt');
    const events = await waitForRemoteSettlement(session.id, receiptId);

    // A used the same fixed coordination contract as a local child.
    const toolNames = events
      .filter((event) => event.type === 'agent.tool_use')
      .map((event) => event.name);
    assert.deepEqual(toolNames, ['list_agents', 'send_to_agent']);
    pass('server A issued fixed list_agents -> send_to_agent coordination');

    // The coordinator ends on the admission receipt, while B's result crosses
    // back through the ordinary child Thread event.
    const coordinator = JSON.stringify(
      events.filter((event) => event.type === 'agent.message').map((event) => event.content),
    );
    assert.ok(coordinator.includes('coordination accepted:'), 'A ended on the send receipt');
    assert.ok(
      coordinator.includes('coordination completed from child report'),
      'A consumed the later child report without another send',
    );
    assert.ok(!coordinator.includes('do the research'), 'the receipt is not B’s synchronous reply');
    const reply = JSON.stringify(
      events.filter((event) => event.type === 'agent.thread_message_received').map((event) => event.content),
    );
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
