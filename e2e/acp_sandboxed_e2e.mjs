// Sandboxed-ACP Managed Agents e2e: `runtime:"acp:*"` sessions launch the ACP
// CLI INSIDE a bubblewrap namespace sandbox (`AWAKEN_MODEL_MODE=acp-sandboxed`),
// and the session's environment networking policy is enforced by the OS. The
// fake agent probes a listener on the HOST loopback and reports what it saw:
// an `unrestricted` environment's CLI reaches it (`net=UP`); an empty official
// `limited` allow-list
// launches under `bwrap --unshare-net`, where even the host loopback is
// unreachable (`net=DOWN`). Self-skips when bwrap/userns is unavailable.
//
// Run: (from e2e/)  node acp_sandboxed_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawnSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, waitForSessionEventReceipt } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38171;

function bwrapAvailable() {
  const r = spawnSync('bwrap', ['--unshare-user', '--ro-bind', '/', '/', '--', 'true'], {
    stdio: 'ignore',
  });
  return r.status === 0;
}

function agentTexts(events) {
  return events
    .filter((e) => e.type === 'agent.message')
    .map((m) => (m.content ?? []).map((c) => c.text ?? '').join('').trim());
}

async function send(client, sessionId, text) {
  // C1=exact ACP/native User receipt; C2=network-probe reply+terminal. E1=C2
  // after C1. K: OS network policy remains the authority. Decision S1 C1&&!C2
  // =>retry; S2 C1+C2=>return the scoped proof.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'S1 exact sandboxed ACP/native User receipt');
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `S1 sandboxed Run for ${JSON.stringify(text)} to commit`,
  );
}

async function acpReply(client, environmentId, prompt) {
  const session = await client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: environmentId,
    betas: BETAS,
  });
  return agentTexts((await send(client, session.id, prompt)).delta);
}

async function main() {
  if (!bwrapAvailable()) {
    console.log('E2E SKIP: bwrap/unprivileged userns unavailable on this host.');
    process.exitCode = 0;
    return;
  }
  // A live listener on the host loopback — the in-sandbox probe's target.
  const listener = net.createServer(() => {});
  await new Promise((resolve) => listener.listen(0, '127.0.0.1', resolve));
  const probePort = listener.address().port;

  const { server, baseUrl } = spawnServer('acp-sandboxed', PORT, {
    AWAKEN_ACP_PROBE_PORT: String(probePort),
  });
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    // Cause-effect graph for the single Session Environment authority:
    // C1 ACP selected -> E1 launch from the Session-owned Environment
    // C1 + C2 unrestricted -> E2 share host networking
    // C1 + C3 empty limited allow-list -> E3 unshare networking and block host-loopback bypass
    // !C1 -> E4 native execution still uses the same Session lifecycle
    //
    // | Rule | Runtime | Network      | Result                         |
    // | S1   | ACP     | unrestricted | bound process, probe reachable |
    // | S2   | ACP     | limited []   | bound process, probe blocked   |
    // | S3   | Native  | unrestricted | native turn succeeds           |

    // Unrestricted networking: the CLI runs OS-confined but shares the host
    // network namespace — the probe reaches the host loopback listener.
    const openEnv = await client.beta.environments.create({
      name: `sbx-open-${PORT}`,
      config: { type: 'cloud', networking: { type: 'unrestricted' } },
      betas: BETAS,
    });
    let texts = await acpReply(client, openEnv.id, 'hello');
    assert.ok(
      texts.some((t) => t.includes('acp-runtime reply net=UP')),
      `unrestricted sandboxed CLI reaches the host loopback, got ${JSON.stringify(texts)}`,
    );
    pass('acp session runs the CLI inside bwrap; unrestricted networking shares the host net');

    // Empty `limited` networking: the same CLI launches under `--unshare-net` — an empty
    // network namespace where the host loopback does not exist.
    const isoEnv = await client.beta.environments.create({
      name: `sbx-iso-${PORT}`,
      config: { type: 'cloud', networking: { type: 'limited', allowed_hosts: [] } },
      betas: BETAS,
    });
    texts = await acpReply(client, isoEnv.id, 'hello');
    assert.ok(
      texts.some((t) => t.includes('acp-runtime reply net=DOWN')),
      `deny-egress environment must confine the CLI, got ${JSON.stringify(texts)}`,
    );
    pass('empty limited network policy OS-confines the ACP CLI: no route even to host loopback');

    // Selection still holds: a native session on the same server runs the model.
    const native = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    texts = agentTexts((await send(client, native.id, 'hello')).delta);
    assert.ok(
      texts.some((t) => t.startsWith('Echo:')),
      `native session ran the built-in model, got ${JSON.stringify(texts)}`,
    );
    pass('native sessions on the same server still run the built-in model');

    console.log('E2E PASS: sandboxed ACP runtime + OS-enforced networking policy.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(server);
    listener.close();
  }
}

main();
