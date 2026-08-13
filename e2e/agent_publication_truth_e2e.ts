// Typed Agent authoring truth -> immutable publication -> Session pin.
//
// This real-process scenario proves that MCP and delegation configuration has
// one mutable source (AgentConfig), while every Session consumes only the last
// publication. A later draft edit cannot leak into execution before publish and
// a republish cannot rewrite an existing Session.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JavaScript harness intentionally serves TS scenarios.
import { pass, withScenarioServer } from './harness.mjs';
// @ts-ignore -- shared JavaScript fixture intentionally serves TS scenarios.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

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

function definition(delegate: string, server: string, url: string): Record<string, unknown> {
  return {
    name: 'Typed coordinator',
    system: 'Coordinate only through the published roster.',
    model: { id: 'fake-haiku' },
    tools: [],
    mcp_servers: [{
      name: server,
      url,
    }],
    multiagent: { type: 'coordinator', agents: [delegate] },
  };
}

async function publishDelegate(base: string, delegate: string): Promise<void> {
  // Roster-closure cause graph: C1 the parent publication names a delegate; C2
  // that delegate has one installed executable profile. C1+C2 => Session pins
  // the closed roster; C1+!C2 => fail `multiagent_unavailable` before Session
  // persistence. FMECA: a name-only compatibility fallback would hide deleted
  // Agents and create execution behavior outside the publication authority.
  const authored = await request(base, 'PUT', `/v1/config/agents/${delegate}`, {
    name: delegate,
    system: 'Act only as an installed leaf delegate.',
    model: { id: 'fake-haiku' },
    tools: [],
  });
  assert.equal(authored.status, 200, JSON.stringify(authored.body));
  const published = await request(base, 'POST', `/v1/config/agents/${delegate}/publish`);
  assert.equal(published.status, 200, JSON.stringify(published.body));
}

async function createSession(client: Anthropic): Promise<any> {
  return client.beta.sessions.create({
    agent: PARENT,
    environment_id: 'env_local',
    betas: BETAS,
  });
}

function assertPinned(session: any, delegate: string, server: string, url: string): void {
  assert.deepEqual(session.agent.multiagent, {
    type: 'coordinator',
    agents: [delegate],
  });
  assert.deepEqual(session.agent.mcp_servers, [{
    name: server,
    type: 'url',
    url,
  }]);
}

async function main(): Promise<void> {
  const fixtureA = await startCalcFixture('unused-a', { allowAnonymous: true });
  const fixtureB = await startCalcFixture('unused-b', { allowAnonymous: true });
  try {
  await withScenarioServer('management', 'mcp', PORT, async (base: string) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    await publishDelegate(base, 'delegate-a');
    await publishDelegate(base, 'delegate-b');

    // Cause C1: canonical tagged roster -> persist the one typed aggregate.
    // Cause C2: removed `{workers}` compatibility shape -> reject at the edge;
    // it must not revive a second authoring vocabulary.
    //
    // | Rule | shape | effect |
    // |---|---|---|
    // | A1 | `{type:"coordinator", agents}` | canonical draft persisted |
    // | A2 | `{workers}` | 400, no draft mutation |
    const canonical = definition('delegate-a', 'docs-a', fixtureA.url);
    const authoredCanonical = await request(base, 'PUT', `/v1/config/agents/${PARENT}`, canonical);
    assert.equal(authoredCanonical.status, 200, JSON.stringify(authoredCanonical.body));
    const authored = await request(base, 'GET', `/v1/config/agents/${PARENT}`);
    assert.deepEqual(authored.body.multiagent, {
      type: 'coordinator',
      agents: ['delegate-a'],
    });
    const legacy = await request(base, 'PUT', `/v1/config/agents/${PARENT}`, {
      ...definition('delegate-b', 'docs-b', fixtureB.url),
      multiagent: { workers: ['delegate-b'] },
    });
    assert.equal(legacy.status, 400, 'A2 rejects the deleted parallel roster shape');
    const afterLegacy = await request(base, 'GET', `/v1/config/agents/${PARENT}`);
    assert.deepEqual(afterLegacy.body.multiagent, authored.body.multiagent, 'A2 preserves A1');
    pass('canonical roster persists; removed workers compatibility fails closed');

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
        { ...definition('delegate-a', 'docs-a', fixtureA.url), multiagent },
      );
      assert.equal(verdict.status, 200);
      assert.equal(verdict.body.valid, false);
      assert.equal(verdict.body.issues[0].path, 'multiagent', JSON.stringify(verdict.body));
    }
    assert.equal(
      (
        await request(
          base,
          'POST',
          `/v1/config/agents/${PARENT}/validate`,
          { ...definition('delegate-a', 'docs-a', fixtureA.url), multiagent: { type: 'mesh', agents: [] } },
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
    assertPinned(first, 'delegate-a', 'docs-a', fixtureA.url);

    // Editing the sole mutable truth does not mutate the installed publication.
    assert.equal(
      (
        await request(
          base,
          'PUT',
          `/v1/config/agents/${PARENT}`,
          definition('delegate-b', 'docs-b', fixtureB.url),
        )
      ).status,
      200,
    );
    const beforeRepublish = await createSession(client);
    assertPinned(beforeRepublish, 'delegate-a', 'docs-a', fixtureA.url);
    pass('draft edits cannot leak into execution before publication');

    const secondPublication = await request(
      base,
      'POST',
      `/v1/config/agents/${PARENT}/publish`,
    );
    assert.equal(secondPublication.status, 200);
    assert.notEqual(secondPublication.body.fingerprint, firstPublication.body.fingerprint);
    const afterRepublish = await createSession(client);
    assertPinned(afterRepublish, 'delegate-b', 'docs-b', fixtureB.url);

    const stillPinned = await client.beta.sessions.retrieve(first.id, { betas: BETAS });
    assertPinned(stillPinned, 'delegate-a', 'docs-a', fixtureA.url);
    pass('republish affects only later Sessions; existing Session remains pinned');

    // The removed aggregate API must not quietly become a second authoring path.
    assert.equal((await request(base, 'GET', '/v1/config/mcp-servers')).status, 404);
    assert.equal(
      (await request(base, 'GET', `/v1/config/agents/${PARENT}/mcp`)).status,
      404,
    );
    pass('legacy MCP aggregate routes are absent');
  });
  } finally {
    await fixtureA.close();
    await fixtureB.close();
  }

  console.log('E2E PASS: typed Agent bindings are the sole mutable truth and Sessions pin publications.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
