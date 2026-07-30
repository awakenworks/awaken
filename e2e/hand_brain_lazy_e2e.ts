// Native Awaken Hand/Brain placement + lazy Sandbox E2E.
//
// Public API actions create one self-hosted Environment, author and bind an
// immutable `on_tool_use` SandboxExecutionPolicy, then run two Sessions:
//   Brain Session: dynamic MCP call -> no durable sandbox binding.
//   Hand Session: built-in read call -> binding committed before tool execution.
// The disposable SQLite aggregate is inspected only as persistence evidence; no
// test-only runtime hook participates in either run.

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore local JS test harness intentionally has no declaration package.
import { E2E_HOME_ROOT, pass, withScenarioServer } from './harness.mjs';
// @ts-ignore local JS fixture intentionally has no declaration package.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { alwaysAllowMcpAgent, sendManagedMessage } from './fixtures/managed_mcp_session.ts';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 39810);

type Aggregate = { environment_binding?: string | null };
type Event = { type: string; name?: string; content?: unknown };

function sessionAggregate(root: string, sessionId: string): Aggregate {
  const id = sessionId.replaceAll("'", "''");
  const databases = execFileSync('find', [root, '-name', 'sessions.db', '-type', 'f'], {
    encoding: 'utf8',
  }).trim().split('\n').filter(Boolean);
  assert.equal(databases.length, 1, `one durable sessions.db under ${root}: ${JSON.stringify(databases)}`);
  const database = databases[0];
  const json = execFileSync(
    'sqlite3',
    ['-cmd', '.timeout 5000', database, `SELECT aggregate_json FROM managed_session WHERE session_id = '${id}'`],
    { encoding: 'utf8' },
  ).trim();
  assert.ok(json, `durable aggregate exists for ${sessionId}`);
  return JSON.parse(json) as Aggregate;
}

async function events(client: Anthropic, sessionId: string): Promise<Event[]> {
  const observed: Event[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    observed.push(event as Event);
  }
  return observed;
}

async function json<T>(base: string, method: string, route: string, body?: unknown): Promise<T> {
  const response = await fetch(`${base}${route}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  assert.ok(response.ok, `${method} ${route}: ${response.status} ${text}`);
  return JSON.parse(text) as T;
}

async function main(): Promise<void> {
  const root = mkdtempSync(path.join(tmpdir(), 'awaken-hand-brain-lazy-'));
  const tier = process.env.SESSION_ENVIRONMENT_TIER ?? 'local';
  const fixture = await startCalcFixture('unused', { allowAnonymous: true });
  try {
    await withScenarioServer(
      'management',
      'handBrainLazy',
      PORT,
      async (base: string) => {
        const environment = await json<{ id: string }>(base, 'POST', '/v1/environments', {
          name: 'native-lazy-e2e',
          config: { type: 'self_hosted' },
        });
        const policyId = `lazy-${process.pid}`;
        const policy = await json<{ id: string; version: number }>(
          base,
          'POST',
          '/v1/awaken/sandbox-execution-policies',
          { id: policyId, config: {}, provisioning: 'on_tool_use', disabled: false },
        );
        await json(base, 'POST', `/v1/awaken/environments/${environment.id}/sandbox-execution-policy`, {
          policy_id: policy.id,
          version: policy.version,
        });
        pass('H1 Environment freezes an exact native on_tool_use Sandbox policy');

        const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
        const calc = { name: 'calc', type: 'url' as const, url: fixture.url };
        const brain = await client.beta.sessions.create({
          agent: alwaysAllowMcpAgent('assistant', [calc]),
          environment_id: environment.id,
          betas: BETAS,
        });
        assert.equal(sessionAggregate(E2E_HOME_ROOT, brain.id).environment_binding, null, 'H2 create is sandbox-free');
        await sendManagedMessage(client, brain.id, 'brain', BETAS);
        const brainEvents = await events(client, brain.id);
        assert.ok(
          brainEvents.some((event) => event.type === 'agent.mcp_tool_use' && event.name === 'mcp__calc__add'),
          `H3 dynamic MCP executed in Brain: ${JSON.stringify(brainEvents.map((event) => event.type))}`,
        );
        assert.equal(sessionAggregate(E2E_HOME_ROOT, brain.id).environment_binding, null, 'H3 MCP never creates Sandbox');
        assert.ok(fixture.calls.some((call: { method: string }) => call.method === 'tools/call'));
        pass('H2/H3 Session creation and dynamic MCP remain sandbox-free');

        const hand = await client.beta.sessions.create({
          agent: 'assistant',
          environment_id: environment.id,
          betas: BETAS,
        });
        assert.equal(sessionAggregate(E2E_HOME_ROOT, hand.id).environment_binding, null, 'H4 Hand Session starts sandbox-free');
        await sendManagedMessage(client, hand.id, 'hand', BETAS);
        const handEvents = await events(client, hand.id);
        assert.ok(
          handEvents.some((event) => event.type === 'agent.tool_use' && event.name === 'read'),
          `H5 read was routed as a Hand tool: ${JSON.stringify(handEvents)}`,
        );
        assert.ok(
          handEvents.some(
            (event) => event.type === 'agent.tool_result'
              && !JSON.stringify(event.content).includes('sandbox executor unavailable'),
          ),
          `H5 deferred Hand executed instead of failing placement: ${JSON.stringify(handEvents)}`,
        );
        const binding = sessionAggregate(E2E_HOME_ROOT, hand.id).environment_binding;
        assert.ok(binding, `H5 binding is durable after the first Hand tool: ${JSON.stringify(handEvents)}`);
        const handle = JSON.parse(binding as string) as { sandbox_id?: string; provider_kind?: string };
        assert.equal(handle.sandbox_id, hand.id, 'H5 Sandbox is Session-owned');
        assert.ok(handle.provider_kind, 'H5 binding records the selected sandbox provider');
        pass('H4/H5 first Hand tool blocks for one Session-owned Sandbox and persists its binding');
      },
      {
        AWAKEN_SANDBOX_DIR: path.join(root, 'sandboxes'),
        SESSION_ENVIRONMENT_TIER: tier,
      },
    );
    console.log('E2E PASS: TypeScript Hand/Brain lazy Sandbox decision table.');
  } finally {
    await fixture.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
