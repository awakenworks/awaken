// Typed Agent authoring truth -> immutable publication -> Session pin.
//
// This real-process scenario proves that MCP and delegation configuration has
// one mutable source (AgentConfig), while every Session consumes only the last
// publication. A later draft edit cannot leak into execution before publish and
// a republish cannot rewrite an existing Session.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { pass, withScenarioServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38641);
const BETAS = ['managed-agents-2026-04-01'];
const PARENT = 'typed-coordinator';

type JsonResult = {
  status: number;
  body: Record<string, any>;
};

async function request(
  base: string,
  method: string,
  route: string,
  body?: unknown,
): Promise<JsonResult> {
  const response = await fetch(`${base}${route}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return {
    status: response.status,
    body: await response.json().catch(() => ({})),
  };
}

function definition(delegate: string, server: string): Record<string, unknown> {
  return {
    name: 'Typed coordinator',
    system: 'Coordinate only through the published roster.',
    model: { id: 'config-model' },
    tools: [],
    mcp_servers: [{
      name: server,
      url: `https://${server}.example.invalid/mcp`,
    }],
    multiagent: { type: 'coordinator', agents: [delegate] },
  };
}

async function createSession(client: Anthropic): Promise<any> {
  return client.beta.sessions.create({
    agent: PARENT,
    environment_id: 'env_local',
    betas: BETAS,
  });
}

function assertPinned(session: any, delegate: string, server: string): void {
  assert.deepEqual(session.agent.multiagent, {
    type: 'coordinator',
    agents: [delegate],
  });
  assert.deepEqual(session.agent.mcp_servers, [{
    name: server,
    type: 'url',
    url: `https://${server}.example.invalid/mcp`,
  }]);
}

async function main(): Promise<void> {
  await withScenarioServer('config', 'instruction', PORT, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // Legacy roster spelling is accepted only at the HTTP anti-corruption edge
    // and immediately projects back as the canonical typed form.
    const legacy = {
      ...definition('delegate-a', 'docs-a'),
      multiagent: { workers: ['delegate-a'] },
    };
    assert.equal((await request(base, 'PUT', `/v1/config/agents/${PARENT}`, legacy)).status, 200);
    const authored = await request(base, 'GET', `/v1/config/agents/${PARENT}`);
    assert.deepEqual(authored.body.multiagent, {
      type: 'coordinator',
      agents: ['delegate-a'],
    });
    pass('legacy roster is normalized at the edge; stored projection is canonical');

    // Domain-invalid rosters reach the validation query as structured issues;
    // an invalid union tag fails decoding at the edge.
    for (const multiagent of [
      { type: 'coordinator', agents: ['delegate-a', 'delegate-a'] },
      { type: 'coordinator', agents: [PARENT] },
      { type: 'coordinator', agents: [''] },
    ]) {
      const verdict = await request(
        base,
        'POST',
        `/v1/config/agents/${PARENT}/validate`,
        { ...definition('delegate-a', 'docs-a'), multiagent },
      );
      assert.equal(verdict.status, 200);
      assert.equal(verdict.body.valid, false);
      assert.equal(verdict.body.issues[0].path, 'multiagent');
    }
    assert.equal(
      (
        await request(
          base,
          'POST',
          `/v1/config/agents/${PARENT}/validate`,
          { ...definition('delegate-a', 'docs-a'), multiagent: { type: 'mesh', agents: [] } },
        )
      ).status,
      400,
    );
    pass('duplicate, self, empty, and unsupported rosters fail closed');

    const firstPublication = await request(
      base,
      'POST',
      `/v1/config/agents/${PARENT}/publish`,
    );
    assert.equal(firstPublication.status, 200);
    assert.ok(firstPublication.body.fingerprint);
    const first = await createSession(client);
    assertPinned(first, 'delegate-a', 'docs-a');

    // Editing the sole mutable truth does not mutate the installed publication.
    assert.equal(
      (
        await request(
          base,
          'PUT',
          `/v1/config/agents/${PARENT}`,
          definition('delegate-b', 'docs-b'),
        )
      ).status,
      200,
    );
    const beforeRepublish = await createSession(client);
    assertPinned(beforeRepublish, 'delegate-a', 'docs-a');
    pass('draft edits cannot leak into execution before publication');

    const secondPublication = await request(
      base,
      'POST',
      `/v1/config/agents/${PARENT}/publish`,
    );
    assert.equal(secondPublication.status, 200);
    assert.notEqual(secondPublication.body.fingerprint, firstPublication.body.fingerprint);
    const afterRepublish = await createSession(client);
    assertPinned(afterRepublish, 'delegate-b', 'docs-b');

    const stillPinned = await client.beta.sessions.retrieve(first.id, { betas: BETAS });
    assertPinned(stillPinned, 'delegate-a', 'docs-a');
    pass('republish affects only later Sessions; existing Session remains pinned');

    // The removed aggregate API must not quietly become a second authoring path.
    assert.equal((await request(base, 'GET', '/v1/config/mcp-servers')).status, 404);
    assert.equal(
      (await request(base, 'GET', `/v1/config/agents/${PARENT}/mcp`)).status,
      404,
    );
    pass('legacy MCP aggregate routes are absent');
  });

  console.log('E2E PASS: typed Agent bindings are the sole mutable truth and Sessions pin publications.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
