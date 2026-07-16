// When-online conformance smoke — the ONLY way to converge behaviors that a local
// server can't reproduce (the matrix's WOL items: retries_exhausted, session.status_
// rescheduled, real workers_polling, etc.). Credential-gated and pre-staged: it runs
// the official SDK against the REAL Anthropic Managed API only when opted in, and
// SKIPS CLEANLY (exit 0) otherwise — so it lives in the tree, ready, without needing
// credentials to be green.
//
// Enable:  AWAKEN_WHEN_ONLINE=1  ANTHROPIC_API_KEY=sk-ant-...  node conformance/when_online_smoke_e2e.mjs
// Default (no creds): prints SKIP and exits 0.
//
// What it checks when online: the real server's outbound event `type`s are a SUBSET
// of awaken's Rust OutboundKind catalog (i.e. awaken models every event the live
// server actually emits) — the doc-vs-real-server cross-check no local oracle gives.

import assert from 'node:assert/strict';
import { rustOutboundTypes, rustInboundTypes } from './catalog.mjs';

const BETAS = ['managed-agents-2026-04-01'];

function enabled() {
  return process.env.AWAKEN_WHEN_ONLINE === '1' && !!process.env.ANTHROPIC_API_KEY;
}

async function runOnline() {
  const { default: Anthropic } = await import('@anthropic-ai/sdk');
  // No baseURL override → the SDK's real https://api.anthropic.com.
  const client = new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY });
  const agent = process.env.AWAKEN_WHEN_ONLINE_AGENT || 'assistant';
  const environmentId = process.env.AWAKEN_WHEN_ONLINE_ENV || 'env_local';

  const session = await client.beta.sessions.create({ agent, environment_id: environmentId, betas: BETAS });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'Say hi in one word.' }] }],
    betas: BETAS,
  });

  const seen = new Set();
  for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) seen.add(ev.type);

  const catalog = new Set([...rustOutboundTypes(), ...rustInboundTypes()]);
  const unmodelled = [...seen].filter((t) => !catalog.has(t));
  console.log('  real-server event types:', [...seen].sort().join(', '));
  assert.deepEqual(
    unmodelled, [],
    `the live server emitted type(s) awaken's OutboundKind/InboundEvent do not model: ${unmodelled.join(', ')}`,
  );
  console.log('WHEN-ONLINE PASS: every live-server event type is modelled by awaken.');
}

async function main() {
  if (!enabled()) {
    console.log('SKIP: when-online smoke not enabled (set AWAKEN_WHEN_ONLINE=1 + ANTHROPIC_API_KEY).');
    console.log('  This suite is pre-staged and intentionally inert without real-API credentials.');
    process.exitCode = 0;
    return;
  }
  try {
    await runOnline();
    process.exitCode = 0;
  } catch (err) {
    console.error('WHEN-ONLINE FAIL:', err.message || err);
    process.exitCode = 1;
  }
}

main();
