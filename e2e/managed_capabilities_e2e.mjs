// Managed Agents capability-advertisement e2e with the official Anthropic TypeScript
// SDK. Proves the created session's agent object carries the *official* wire shapes:
// the built-in tools fold into one `agent_toolset_20260401` reference, client tools
// are `custom` definitions, a delegate roster is a `coordinator` multiagent object,
// and mcp_servers / resources are empty. The SDK deserializes the session at each
// step, so a shape the SDK can't place would surface here.
//
// Run: (from e2e/)  npm install && node managed_capabilities_e2e.mjs

import assert from 'node:assert/strict';
import net from 'node:net';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
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

// Spawn the server in `mode` on `port`, run `fn(client)`, then stop it.
async function withServer(mode, port, fn) {
  const addr = `127.0.0.1:${port}`;
  const server = spawn('cargo', ['run', '--quiet', '-p', 'awaken-server-local'], {
    cwd: REPO_ROOT,
    env: { ...process.env, AWAKEN_HTTP_ADDR: addr, AWAKEN_MODEL_MODE: mode },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  try {
    await waitForPort(port, 180_000);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
    await fn(client);
  } finally {
    server.kill('SIGINT');
  }
}

function toolset(session) {
  const t = session.agent.tools.find((x) => x.type === 'agent_toolset_20260401');
  assert.ok(t, `agent_toolset_20260401 present in ${JSON.stringify(session.agent.tools)}`);
  return t;
}

async function main() {
  try {
    // --- echo: built-in toolset fold, empty everything else ---
    await withServer('echo', 38101, async (client) => {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      const ts = toolset(s);
      // Confirmation-gated built-ins are always_ask; unregistered ones are disabled.
      const cfg = Object.fromEntries(ts.configs.map((c) => [c.name, c]));
      assert.deepEqual(cfg.bash.permission_policy, { type: 'always_ask' });
      assert.deepEqual(cfg.write.permission_policy, { type: 'always_ask' });
      assert.equal(cfg.web_fetch.enabled, false);
      assert.equal(cfg.web_search.enabled, false);
      // read/glob/grep are auto-allowed → not present in configs (toolset default).
      assert.ok(!('read' in cfg) && !('glob' in cfg) && !('grep' in cfg));
      assert.deepEqual(s.agent.mcp_servers, []);
      assert.deepEqual(s.agent.skills, []);
      assert.deepEqual(s.resources, []);
      assert.ok(s.agent.multiagent == null, 'no multiagent without a roster');
      console.log('  ok: echo -> agent_toolset fold, empty mcp/skills/resources, no multiagent');
    });

    // --- custom: a client tool appears as a `custom` tool definition ---
    await withServer('custom', 38102, async (client) => {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      toolset(s); // built-in toolset still present
      const custom = s.agent.tools.find((x) => x.type === 'custom');
      assert.ok(custom, `a custom tool is advertised in ${JSON.stringify(s.agent.tools)}`);
      assert.equal(custom.name, 'submit_answer');
      assert.equal(typeof custom.input_schema, 'object');
      console.log('  ok: custom -> {type:custom, name:submit_answer, input_schema}');
    });

    // --- delegate: the roster becomes a coordinator multiagent object ---
    await withServer('delegate', 38103, async (client) => {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      assert.equal(s.agent.multiagent.type, 'coordinator');
      assert.ok(s.agent.multiagent.agents.includes('researcher'), `roster: ${JSON.stringify(s.agent.multiagent.agents)}`);
      console.log('  ok: delegate -> {multiagent:{type:coordinator, agents:[researcher]}}');
    });

    console.log('E2E PASS: Managed Agents capability advertisement matches the official wire via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
