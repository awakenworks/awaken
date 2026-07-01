// Outcome end-to-end test with the official Anthropic TypeScript SDK: a
// `user.define_outcome` drives a grade->revise loop until satisfied, emitting
// `span.outcome_evaluation_start/end` with `needs_revision` then `satisfied`.
//
// Uses the revise server (AWAKEN_MODEL_MODE=revise): reply "a rough draft", then
// "FINAL answer" once it sees the goal loop's feedback; the keyword grader is
// satisfied when the deliverable contains the rubric text.
//
// Run: (from e2e/)  npm install && node managed_outcome_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38103);
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
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(ev);
  }
  return events;
}

async function main() {
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: ADDR, AWAKEN_MODEL_MODE: 'revise' },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  server.on('exit', (code) => {
    if (code !== null && code !== 0) {
      console.error(`server exited early with code ${code}`);
      process.exit(1);
    }
  });

  try {
    await waitForPort(PORT, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${ADDR}` });

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });

    // A draft answer.
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write something' }] }],
      betas: BETAS,
    });

    // Define an outcome the draft misses -> revise -> satisfied.
    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.define_outcome',
        description: 'finish it',
        rubric: { type: 'text', content: 'FINAL' },
        max_iterations: 3,
      }],
      betas: BETAS,
    });

    const events = await listEvents(client, session.id);
    const ends = events.filter((e) => e.type === 'span.outcome_evaluation_end');
    assert.ok(ends.length >= 2, `expected >=2 evaluation rounds, got ${ends.length}`);
    assert.equal(ends[0].result, 'needs_revision');
    assert.equal(ends[ends.length - 1].result, 'satisfied');

    console.log('E2E PASS: Anthropic TS SDK drove the define_outcome grade->revise loop.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    server.kill('SIGINT');
  }
}

main();
