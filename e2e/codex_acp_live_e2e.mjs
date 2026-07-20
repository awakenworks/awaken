// Real Codex ACP end-to-end gate. This test deliberately does not start a fake
// agent: point it at an Awaken server configured with a real Codex ACP adapter,
// then prove Managed Agents -> ACP JSON-RPC -> Codex -> committed transcript.
//
// The server must be built with `container-docker`, use AWAKEN_SANDBOX_TIER=docker,
// and point AWAKEN_CONTAINER_IMAGE at the production sandbox image. Authentication
// must be explicitly injected/brokered for the container; host ~/.codex inheritance
// is not acceptable evidence.
//
//   CODEX_ACP_LIVE=1 CODEX_ACP_CONTAINER=1 \
//     CODEX_ACP_BASE_URL=http://127.0.0.1:38080 \
//     node e2e/codex_acp_live_e2e.mjs

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';

if (process.env.CODEX_ACP_LIVE !== '1') {
  throw new Error('set CODEX_ACP_LIVE=1 to confirm this test may invoke the real Codex ACP adapter');
}
if (process.env.CODEX_ACP_CONTAINER !== '1') {
  throw new Error('set CODEX_ACP_CONTAINER=1 and run the backend with AWAKEN_SANDBOX_TIER=docker');
}

function managedContainerIds() {
  const result = spawnSync('docker', ['ps', '-q', '--filter', 'label=awaken.sandbox=1'], {
    encoding: 'utf8',
  });
  assert.equal(result.status, 0, `Docker is required for this gate: ${result.stderr || result.error || ''}`);
  return new Set(result.stdout.trim().split(/\s+/).filter(Boolean));
}

const baseURL = process.env.CODEX_ACP_BASE_URL ?? 'http://127.0.0.1:38080';
const BETAS = ['managed-agents-2026-04-01'];
const marker = `CODEX-ACP-READY-${Date.now()}`;
const client = new Anthropic({
  apiKey: 'local-live-gate', // awaken-allow: secret (dummy; the local no-auth server ignores it)
  baseURL,
});

async function events(sessionId) {
  const out = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    out.push(event);
  }
  return out;
}

const session = await client.beta.sessions.create({
  agent: 'assistant',
  metadata: { 'awaken.runtime': 'acp:codex' },
  environment_id: 'env_local',
  betas: BETAS,
});

const baselineContainers = managedContainerIds();
const observedContainers = new Set();
const containerProbe = setInterval(() => {
  for (const id of managedContainerIds()) {
    if (!baselineContainers.has(id)) observedContainers.add(id);
  }
}, 100);
try {
  await client.beta.sessions.events.send(session.id, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: `Reply with exactly this text and nothing else: ${marker}` }],
    }],
    betas: BETAS,
  });
} finally {
  clearInterval(containerProbe);
}

assert.ok(
  observedContainers.size > 0,
  'the Codex reply completed without observing a newly-created awaken.sandbox Docker container',
);

const transcript = await events(session.id);
const replies = transcript
  .filter((event) => event.type === 'agent.message')
  .map((event) => (event.content ?? []).map((content) => content.text ?? '').join('').trim());

assert.ok(
  replies.some((reply) => reply.includes(marker)),
  `real Codex ACP reply did not contain the marker; replies=${JSON.stringify(replies)}`,
);
assert.ok(
  transcript.some((event) => event.type === 'session.status_running'),
  `the managed transcript did not expose a running state; events=${transcript.map((event) => event.type)}`,
);
assert.ok(
  transcript.some((event) => event.type === 'session.status_idle'),
  `the managed transcript did not return to idle; events=${transcript.map((event) => event.type)}`,
);

console.log(`CODEX ACP CONTAINER E2E PASS: ${session.id} committed ${marker}`);
