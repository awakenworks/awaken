// T25-C01 (归属与权限) through the real production `awaken` process and real HTTP.
//
// Oracles (groups-4.json): O0 a foreign memory version can be neither read nor
// redacted; O1 the refusal does not reveal whether the foreign resource exists
// (status + body identical to a nonexistent id); O2 a refusal changes nothing,
// also across a process restart.
//
// | Row | Caller workspace | Route store | Version | Expected |
// |---|---|---|---|---|
// | X1/X2 | A | B (A's sibling) | A's / nonexistent | identical 404 |
// | X3/X4 | B (other tenant) | C (B's own) | A's / nonexistent | identical refusal |
// | X5/X6 | B | A's store / nonexistent store | A's / nonexistent | identical refusal |
// | R1 | A after restart | A | A's | content intact, not redacted |
// | P1 | A | A (owner) | A's | 200 redact (the id was real) |
//
// Negative control (executed): the same checker is fed an observation in which
// the foreign redact succeeded and must report O0/O1/O2 violations.
//
// Run: E2E_PORT=<port> node e2e/managed_memory_version_ownership_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {
  cleanupFixtureTree,
  spawnProduction,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38471);
const WORKSPACE_A = `t25-owner-${process.pid}`;
const WORKSPACE_B = `t25-other-${process.pid}`;
const MEMORY_BETA = 'agent-memory-2026-07-22';
const SECRET = `t25 secret ${process.pid}`;
const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-t25-'));
const data = path.join(directory, 'server-data');

const launch = () => spawnProduction(data, PORT, {
  workspace: WORKSPACE_A,
  controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
  fields: { sandbox_tier: 'local' },
});

async function request(workspace, method, tail, body) {
  const response = await fetch(
    `http://127.0.0.1:${PORT}/v1/workspaces/${encodeURIComponent(workspace)}/${tail}`,
    {
      method,
      headers: {
        'anthropic-beta': MEMORY_BETA,
        ...(body === undefined ? {} : { 'content-type': 'application/json' }),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    },
  );
  const text = await response.text();
  return { status: response.status, text };
}

async function ok(workspace, method, tail, body) {
  const answer = await request(workspace, method, tail, body);
  assert.equal(answer.status, 200, `${method} ${tail}: ${answer.text}`);
  return JSON.parse(answer.text);
}

const fullLog = (workspace, store) =>
  request(workspace, 'GET', `memory_stores/${store}/memory_versions?view=full`);

// The T25-C01 checker over one observation; returns every violation.
export function ownershipViolations(observation) {
  const violations = [];
  for (const { row, verb, foreign, absent } of observation.probes) {
    if (foreign.status < 400) violations.push(`${row}/O0 foreign ${verb} answered ${foreign.status}`);
    if (foreign.text.includes(SECRET)) violations.push(`${row}/O0 foreign ${verb} disclosed content`);
    if (foreign.status !== absent.status || foreign.text !== absent.text) {
      violations.push(`${row}/O1 foreign ${verb} ${foreign.status} ${foreign.text} differs from nonexistent ${absent.status} ${absent.text}`);
    }
  }
  for (const [store, log] of observation.foreignLogs) {
    if (log.includes(observation.versionA) || log.includes(SECRET)) {
      violations.push(`O0 store ${store} lists the foreign version`);
    }
  }
  if (JSON.stringify(observation.before) !== JSON.stringify(observation.after)) {
    violations.push('O2 a refused request changed a version log');
  }
  return violations;
}

async function observe(stores, versionA) {
  const { a, b, c } = stores;
  const missingVersion = 'memver_t25_does_not_exist';
  const missingStore = 'memstore_t25_does_not_exist';
  const logs = async () => [
    await fullLog(WORKSPACE_A, a),
    await fullLog(WORKSPACE_A, b),
    await fullLog(WORKSPACE_B, c),
  ];
  const before = await logs();
  const probes = [];
  for (const [row, workspace, store, controlStore] of [
    ['X1/X2', WORKSPACE_A, b, b],
    ['X3/X4', WORKSPACE_B, c, c],
    ['X5/X6', WORKSPACE_B, a, missingStore],
  ]) {
    for (const [verb, method, suffix] of [['read', 'GET', ''], ['redact', 'POST', '/redact']]) {
      probes.push({
        row,
        verb,
        foreign: await request(workspace, method, `memory_stores/${store}/memory_versions/${versionA}${suffix}`),
        absent: await request(workspace, method, `memory_stores/${controlStore}/memory_versions/${missingVersion}${suffix}`),
      });
    }
  }
  const after = await logs();
  return {
    versionA,
    before,
    after,
    probes,
    foreignLogs: [[b, after[1].text], [c, after[2].text]],
  };
}

let server;
try {
  server = launch();
  await waitForPort(PORT, 180_000, server);

  const a = (await ok(WORKSPACE_A, 'POST', 'memory_stores', { name: 't25-owner' })).id;
  const b = (await ok(WORKSPACE_A, 'POST', 'memory_stores', { name: 't25-sibling' })).id;
  // The second tenant's own store. If this deployment refuses the second
  // Workspace path entirely, X3-X6 still require identical refusals.
  const other = await request(WORKSPACE_B, 'POST', 'memory_stores', { name: 't25-other' });
  const c = other.status === 200 ? JSON.parse(other.text).id : 'memstore_t25_tenant_b_unavailable';
  const versionA = (await ok(WORKSPACE_A, 'POST', `memory_stores/${a}/memories`, { path: '/secret.md', content: SECRET })).memory_version_id;
  await ok(WORKSPACE_A, 'POST', `memory_stores/${b}/memories`, { path: '/sibling.md', content: 'sibling' });
  if (other.status === 200) {
    await ok(WORKSPACE_B, 'POST', `memory_stores/${c}/memories`, { path: '/tenant.md', content: 'tenant' });
  }

  const observation = await observe({ a, b, c }, versionA);
  assert.deepEqual(ownershipViolations(observation), [], 'T25-C01 O0-O2 on the production service');
  for (const probe of observation.probes.filter((p) => p.row === 'X1/X2')) {
    assert.equal(probe.foreign.status, 404, `X1 ${probe.verb} is 404`);
  }

  // NC: the same checker must reject an observation where the foreign redact
  // succeeded (store-unscoped lookup by version id alone).
  const leaked = structuredClone(observation);
  const redact = leaked.probes.find((p) => p.row === 'X1/X2' && p.verb === 'redact');
  redact.foreign = { status: 200, text: JSON.stringify({ id: versionA, redacted_at: 'now', content: null }) };
  leaked.after = [{ status: 200, text: leaked.after[0].text.replace(SECRET, '') }, ...leaked.after.slice(1)];
  const detected = ownershipViolations(leaked);
  for (const oracle of ['X1/X2/O0', 'X1/X2/O1', 'O2']) {
    assert.ok(detected.some((v) => v.startsWith(oracle)), `NC must report ${oracle}: ${detected}`);
  }

  // R1: the refusals left nothing behind across a real process restart.
  await stopServer(server);
  server = launch();
  await waitForPort(PORT, 180_000, server);
  const restarted = JSON.parse((await fullLog(WORKSPACE_A, a)).text);
  assert.equal(restarted.data.length, 1, 'R1 one version');
  assert.equal(restarted.data[0].id, versionA, 'R1 same version id');
  assert.equal(restarted.data[0].content, SECRET, 'R1 content intact');
  assert.equal(restarted.data[0].redacted_at, null, 'R1 not redacted');

  // P1: the refused id is real and redactable by its owner.
  const redacted = await ok(WORKSPACE_A, 'POST', `memory_stores/${a}/memory_versions/${versionA}/redact`);
  assert.equal(redacted.id, versionA, 'P1 owner redact');
  assert.notEqual(redacted.redacted_at, null, 'P1 owner redact stamps');
  assert.equal(redacted.content ?? null, null, 'P1 owner redact drops content');
  console.log(`PASS T25-C01 production memory version ownership (tenant B store: ${other.status})`);
} catch (error) {
  console.error(`E2E FAIL: ${error.stack}`);
  process.exitCode = 1;
} finally {
  if (server) await stopServer(server);
  cleanupFixtureTree(data);
  fs.rmSync(directory, { recursive: true, force: true });
}
