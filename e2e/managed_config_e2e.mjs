// Config data plane end-to-end (slice A): author → validate → publish → run.
//
// Over the `config` server (an in-memory SQLite config store + `/v1/config/*`),
// we PUT a declarative agent config, validate it, publish it (compile → store
// publication → install into the live catalog), then create a managed session for
// that agent id and run one turn. The `config` server's model echoes the system
// prompt, so the reply proves the *published agent's own instructions* reached the
// run — i.e. the config→compile→install→execute loop works end to end via HTTP.
//
// Run: (from e2e/)  node managed_config_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38160);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

const AGENT = 'greeter';
const GREETING = 'HELLO-FROM-CONFIG';
const agentConfig = {
  id: AGENT,
  instructions: GREETING,
  max_steps: 4,
  model_binding: { provider_instance_ref: 'default', model_ref: 'config-model', backend_ref: 'default' },
  tool_ids: [],
  plugin_ids: [],
  plugin_config: {},
};

const json = async (method, path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};

async function main() {
  const { server } = spawnServer('config', PORT);
  await waitForPort(PORT);
  try {
    // Author.
    const put = await json('PUT', `/v1/config/agents/${AGENT}`, agentConfig);
    assert.equal(put.status, 200, 'config stored');
    pass('agent config stored');

    // Validate (compile dry-run).
    const valid = await json('POST', `/v1/config/agents/${AGENT}/validate`, agentConfig);
    assert.equal(valid.status, 200, 'config validates');
    assert.equal(valid.body.valid, true, 'valid=true');
    pass('config validated (compiles)');

    // Reject a bad config (unknown tool) — the validation path fails closed.
    const bad = await json('POST', `/v1/config/agents/${AGENT}/validate`, { ...agentConfig, tool_ids: ['no_such_tool'] });
    assert.equal(bad.status, 400, 'bad config rejected');
    pass('invalid config rejected (unknown tool)');

    // Publish (compile → store publication → install).
    const published = await json('POST', `/v1/config/agents/${AGENT}/publish`, undefined);
    assert.equal(published.status, 200, 'published');
    assert.ok(published.body.fingerprint, 'publication has a fingerprint');
    assert.equal(published.body.installed, true, 'installed into the live catalog');
    pass(`published: fingerprint ${published.body.fingerprint.slice(0, 12)}…`);

    // Run: a session for the published agent runs with its own instructions.
    const session = await client.beta.sessions.create({ agent: AGENT, environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hi' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
    const assistant = events.filter((e) => e.type === 'agent.message');
    const text = JSON.stringify(assistant.map((m) => m.content));
    assert.ok(text.includes(GREETING), `run used the published agent's instructions (${GREETING})`);
    pass('published agent ran with its own instructions');

    console.log('E2E PASS: config author→validate→publish→run loop via HTTP + TS SDK.');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
