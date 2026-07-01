// HITL end-to-end with the official Anthropic TypeScript SDK: a mutating tool
// parks for approval (requires_action + agent.tool_use{ask}); the client sends a
// `user.tool_confirmation`. Covers BOTH the allow path (tool runs, read-back
// succeeds) and the deny path (tool is blocked, the run still completes).
//
// Uses the probe server (AWAKEN_MODEL_MODE=probe): write probe.txt (asked), read
// it back (allowed), reply.
//
// Run: (from e2e/)  npm install && node managed_hitl_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38102);
const ADDR = `127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

function waitForPort(port, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    const attempt = () => {
      const sock = net.createConnection({ port, host: '127.0.0.1' });
      sock.once('connect', () => { sock.destroy(); resolve(); });
      sock.once('error', () => {
        sock.destroy();
        if (Date.now() > deadline) reject(new Error(`server did not listen on ${port}`));
        else setTimeout(attempt, 200);
      });
    };
    attempt();
  });
}

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function newSession(client) {
  return client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
}

async function main() {
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR, AWAKEN_MODEL_MODE: 'probe' },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) { console.error(`server exited early: ${code}`); process.exit(1); }
  });

  try {
    await waitForPort(PORT, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${ADDR}` });

    // ---------- allow path ----------
    {
      const session = await newSession(client);
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'HELLO-ALLOW' }] }],
        betas: BETAS,
      });
      let events = await listEvents(client, session.id);
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.ok(toolUse, 'expected agent.tool_use');
      assert.equal(toolUse.evaluated_permission, 'ask');
      const idle = events.find((e) => e.type === 'session.status_idle');
      assert.equal(idle.stop_reason.type, 'requires_action');
      assert.ok(idle.stop_reason.event_ids.includes(toolUse.id));

      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
        betas: BETAS,
      });
      events = await listEvents(client, session.id);
      const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
      assert.equal(lastIdle.stop_reason.type, 'end_turn');
      const results = events.filter((e) => e.type === 'agent.tool_result');
      assert.ok(JSON.stringify(results.at(-1).content).includes('HELLO-ALLOW'), 'read-back after allow');
      console.log('  ok: allow -> tool runs, read-back succeeds');
    }

    // ---------- deny path ----------
    {
      const session = await newSession(client);
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'HELLO-DENY' }] }],
        betas: BETAS,
      });
      let events = await listEvents(client, session.id);
      const toolUse = events.find((e) => e.type === 'agent.tool_use');
      assert.equal(events.find((e) => e.type === 'session.status_idle').stop_reason.type, 'requires_action');

      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'deny', deny_message: 'not allowed' }],
        betas: BETAS,
      });
      events = await listEvents(client, session.id);
      // The run resumes and reaches a terminal turn even though the tool was denied.
      const lastIdle = [...events].reverse().find((e) => e.type === 'session.status_idle');
      assert.equal(lastIdle.stop_reason.type, 'end_turn');
      // The write was blocked, so the read-back does NOT contain the text.
      const results = events.filter((e) => e.type === 'agent.tool_result');
      assert.ok(!JSON.stringify(results.map((r) => r.content)).includes('HELLO-DENY'), 'deny should block the write');
      console.log('  ok: deny -> tool blocked, run still completes');
    }

    console.log('E2E PASS: HITL allow + deny round-trips via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
