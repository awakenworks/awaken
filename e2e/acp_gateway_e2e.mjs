// ACP cloud-managed gateway egress (D-R2, ADR-0021 §9/R2) end to end.
//
// An ACP CLI runs inside an untrusted sandbox, so it must NEVER hold a raw provider
// key: when the placement injects AWAKEN_ACP_GATEWAY_URL + AWAKEN_ACP_LEASE_TOKEN,
// the dev-only scenario resolver points the launched CLI at the GATEWAY with a
// short-lived LEASE TOKEN in the key slot (never the raw ANTHROPIC_API_KEY, even
// though the scenario harness exports one). Product composition instead consumes
// a published gateway endpoint/credential pin. Here the fake ACP adapter stand-in
// echoes the env it was launched with (base URL + key prefix — not the full secret),
// so the managed-API client can assert: base == the gateway, key prefix == `lease-`.
//
// Run: (from e2e/)  node acp_gateway_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass, waitForSessionEventReceipt } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const GATEWAY = 'https://gw.e2e.internal/anthropic';
const LEASE = 'lease-e2e-token'; // awaken-allow: secret

function agentTexts(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function send(client, sessionId, text) {
  // C1=exact ACP User receipt; C2=gateway echo+terminal. E1=C2 after C1.
  // K: environment capability remains process-private. Decision G1 C1&&!C2
  // =>retry; G2 C1+C2=>return the gateway proof.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'G1 exact ACP gateway User Event receipt');
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    'G1 ACP gateway Run to commit its environment proof',
  );
}

async function main() {
  try {
    // The gateway env is injected at server launch (as a placement would), so the
    // ACP CLI is resolved onto the gateway with a lease token.
    const extraEnv = { AWAKEN_ACP_GATEWAY_URL: GATEWAY, AWAKEN_ACP_LEASE_TOKEN: LEASE };
    await withScenarioServer('acp-gateway', 'echo', 38195, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Cause-effect graph:
      // C1 projected resolver returns endpoint + opaque process-secret reference
      // C2 its broker resolves that exact reference at spawn
      // C1 + C2 -> E1 bound ACP process gets gateway URL + lease capability
      // C1 + !C2 -> E2 launch fails closed (never fall back to host env/plaintext)
      //
      // | Rule | Requirement | Broker | Result                         |
      // | G1   | present     | exact  | gateway + lease injected       |
      // | G2   | present     | absent | launch rejected, no fallback   |
      const acp = await client.beta.sessions.create({
        // Select the exact adapter row this scenario serves. A different `acp:*`
        // binding must fail closed instead of being silently executed by this worker.
        agent: 'acp-agent',
        environment_id: 'env_local',
        betas: BETAS,
      });
      // The prompt triggers the fake CLI to echo the env it was launched with.
      const texts = agentTexts((await send(client, acp.id, 'please acp-echo-env')).delta);
      const echoed = texts.find((t) => t.includes('acp-env'));
      assert.ok(echoed, `expected an env echo from the ACP CLI, got ${JSON.stringify(texts)}`);

      // D-R2: the CLI was pointed at the GATEWAY, not a raw provider endpoint.
      assert.ok(
        echoed.includes(`base=${GATEWAY}`),
        `the CLI must dial the gateway; got ${JSON.stringify(echoed)}`,
      );
      // D-R2: its "key" is the short-lived LEASE token (prefix `lease-`), not a raw
      // provider key (the harness's ANTHROPIC_API_KEY is `sk-...`).
      assert.ok(
        echoed.includes('keypfx=lease-'),
        `the CLI must hold a lease token, not a raw key; got ${JSON.stringify(echoed)}`,
      );
      assert.ok(
        !echoed.includes('keypfx=sk-'),
        `the raw provider key must never reach the sandbox; got ${JSON.stringify(echoed)}`,
      );
      pass('D-R2: the cloud-managed ACP CLI dials the gateway with a lease token, never a raw provider key');
    }, extraEnv);

    console.log('\nACP GATEWAY E2E PASS: cloud-managed egress puts the gateway URL + lease token (not a raw key) into the sandboxed ACP CLI.');
  } catch (err) {
    console.error(`\nACP GATEWAY E2E FAIL: ${err.stack ?? err}`);
    process.exit(1);
  }
}

main();
