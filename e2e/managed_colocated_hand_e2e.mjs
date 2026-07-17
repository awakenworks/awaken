// Co-located hand over a unix rendezvous end-to-end (C5, ADR-0044/0045) through the
// served binary + the REAL `awaken-sandbox hand` execution-plane binary.
//
// The co-located topology is the one that has no network: the hand runs inside a
// run's `--network none` sandbox container, so no TCP port can be published. The
// only transport across that boundary is a unix socket the hand binds in a shared
// host<->container rendezvous dir; the brain dials `DialAddr::Unix`. This e2e drives
// that exact wire WITHOUT a container (the socket path stands in for the bind-mount):
//
//   [brain: AWAKEN_MODEL_MODE=remote-hand, AWAKEN_REMOTE_HAND_UNIX=<sock>]
//        --unix socket-->  [awaken-sandbox hand --unix <sock>]
//
// The driving model calls `bash` to echo a fixed marker; the real hand binary runs
// it out of the loop and its stdout round-trips back through the unix channel to the
// brain, which echoes it. Seeing the marker proves the brain's Unix-dial branch, the
// hand binary's serve loop, and the executable tools all wire end to end.
//
// Run: (from e2e/)  npm install && node managed_colocated_hand_e2e.mjs

import assert from 'node:assert/strict';
import os from 'node:os';
import path from 'node:path';
import fs from 'node:fs';
import { spawn, execSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import { REPO_ROOT, withServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38142);
const BETAS = ['managed-agents-2026-04-01'];
const MARKER = 'REMOTE-HAND-OK-9f31'; // must match models.rs REMOTE_HAND_MARKER

// Build the real execution-plane hand binary (feature `hand`) once and resolve its
// path — we spawn the binary directly, exactly as a co-located sidecar would run it.
function ensureHandBin() {
  const out = execSync(
    'cargo build --quiet --message-format=json -p awaken-sandbox --bin awaken-sandbox --features hand',
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of out.split('\n')) {
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch {
      continue;
    }
    if (msg.executable && msg.target?.name === 'awaken-sandbox') return msg.executable;
  }
  throw new Error('could not resolve the awaken-sandbox binary path');
}

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function main() {
  const rv = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-colocated-hand-'));
  const sock = path.join(rv, 'hand.sock');

  const handBin = ensureHandBin();
  const hand = spawn(handBin, ['hand', '--unix', sock], { stdio: ['ignore', 'inherit', 'inherit'] });
  let handExited = false;
  hand.on('exit', () => {
    handExited = true;
  });

  // The brain dials the hand's socket; point its remote-hand mode at the rendezvous.
  process.env.AWAKEN_REMOTE_HAND_UNIX = sock;

  try {
    // Wait for the hand to bind the socket before the brain starts (it also retries).
    for (let i = 0; i < 100 && !fs.existsSync(sock); i++) {
      if (handExited) throw new Error('the hand binary exited before binding its socket');
      await new Promise((r) => setTimeout(r, 50));
    }
    assert.ok(fs.existsSync(sock), 'the hand binary must bind its unix socket');

    await withServer('remote-hand', PORT, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });

      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'run the hand' }] }],
        betas: BETAS,
      });

      const events = await listEvents(client, session.id);

      // A server-side tool executed on the co-located hand (a real tool_use event).
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.ok(toolUse, `expected a server-side tool_use, got: ${events.map((e) => e.type)}`);

      // The hand binary's `bash echo` stdout round-tripped over the unix channel and
      // the model echoed it — the brain→(unix)→hand→brain path through real binaries.
      const messages = events
        .filter((e) => e.type === 'agent.message')
        .map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
      assert.ok(
        messages.some((m) => m.includes(MARKER)),
        `the co-located hand's bash output must round-trip: ${JSON.stringify(messages)}`,
      );

      const idle = [...events].reverse().find((e) => e.type === 'session.status_idle');
      assert.equal(idle.stop_reason.type, 'end_turn');

      console.log(
        'E2E PASS: co-located hand — a served run executed bash on the real awaken-sandbox hand over a unix rendezvous and its output round-tripped (C5, ADR-0044).',
      );
    });
  } finally {
    delete process.env.AWAKEN_REMOTE_HAND_UNIX;
    hand.kill('SIGINT');
    fs.rmSync(rv, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
