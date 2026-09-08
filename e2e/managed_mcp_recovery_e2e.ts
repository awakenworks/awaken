// ADR-0066 durable MCP generation recovery through a real process restart.
//
// Cause-effect graph:
//   C1 durable generation Active + owner lost -> new lease/claim -> E1 restage and republish exact generation
//   C2 exact idempotency receipt persisted -> retry after restart -> E2 replay without new effect
//   C3 durable generation Removed + restart -> E3 never republish
//
// Decision table:
// | Rule | durable state | trigger | Effect |
// | R0 | idle baseline, no transcript | restart/retrieve | reconstruct idle Session |
// | R1 | Active B | SIGKILL + restart | same Session calls only B |
// | R2 | Active B + update receipt | replay key after restart | same ETag, no MCP I/O/event |
// | R3 | Removed B | SIGKILL + restart | empty projection, no B resurrection |

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore -- shared JS harness deliberately serves TS restart scenarios.
import { availablePort, pass, hardKillServer, waitForValue, realServerEnv, spawnServer, startUpstream, stopServer, waitForPort } from './harness.mjs';
// @ts-ignore -- shared JS fixture deliberately serves TS scenarios.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import {
  alwaysAllowMcpAgent,
  type McpServer,
  replaceMcpServers,
  responseEtag,
  retrieveSessionWithEtag,
  sendManagedMessage,
} from './fixtures/managed_mcp_session.ts';

const BETAS = ['managed-agents-2026-04-01'];
const PREFERRED_PORT = Number(process.env.E2E_PORT);
const TOKEN_A = 'recovery-token-a'; // awaken-allow: secret
const TOKEN_B = 'recovery-token-b'; // awaken-allow: secret

async function update(
  client: Anthropic,
  sessionId: string,
  servers: McpServer[],
  key: string,
  ifMatch?: string,
) {
  return replaceMcpServers(client, sessionId, servers, BETAS, {
    'Idempotency-Key': key,
    ...(ifMatch === undefined ? {} : { 'If-Match': ifMatch }),
  });
}

async function main(): Promise<void> {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-mcp-recovery-'));
  const port = await availablePort(PREFERRED_PORT);
  const fixtureA = await startCalcFixture(TOKEN_A);
  const fixtureB = await startCalcFixture(TOKEN_B);
  const upstream = await startUpstream('mcp');
  const observationDir = path.join(root, 'realization-observations');
  fs.mkdirSync(observationDir);
  const env = {
    AWAKEN_REALIZATION_OBSERVATION_DIR: observationDir,
    SESSION_DEPLOYMENT_STORAGE_DIR: path.join(root, 'runtime'),
    ...realServerEnv('mcp', upstream, { mode: 'management' }),
  };
  const caseFixtures: any[] = [];
  let server: any = null;
  try {
    let started = spawnServer('management', port, env);
    server = started.server;
    let baseUrl = started.baseUrl;
    await waitForPort(port, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const vault = await client.beta.vaults.create({ display_name: 'recovery', betas: BETAS });
    for (const [url, token] of [[fixtureA.url, TOKEN_A], [fixtureB.url, TOKEN_B]]) {
      await client.beta.vaults.credentials.create(vault.id, {
        auth: { type: 'static_bearer', mcp_server_url: url, token },
        betas: BETAS,
      });
    }
    const serverA: McpServer = { name: 'calc', type: 'url', url: fixtureA.url };
    const serverB: McpServer = { name: 'calc', type: 'url', url: fixtureB.url };
    const session = await client.beta.sessions.create({
      agent: alwaysAllowMcpAgent('assistant', [serverA]),
      environment_id: 'env_local',
      vault_ids: [vault.id],
      betas: BETAS,
    });
    const emptySession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const replaced = await update(client, session.id, [serverB], 'persisted-swap-b');
    const replaceEtag = responseEtag(replaced.response);
    await sendManagedMessage(client, session.id, 'add 2 5', BETAS);
    assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 1);

    await hardKillServer(server, "MCP durable recovery");
    server = null;
    started = spawnServer('management', port, env);
    server = started.server;
    baseUrl = started.baseUrl;
    await waitForPort(port, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

    const recoveredEmpty = await client.beta.sessions.retrieve(emptySession.id, { betas: BETAS });
    assert.equal(recoveredEmpty.status, 'idle', 'R0 reconstructs a transcript-free Session');
    pass('R0 durable idle Session survives restart without requiring transcript rows');
    const recovered = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.deepEqual(recovered.agent.mcp_servers, [serverB], 'R1 restores durable active B');
    await sendManagedMessage(client, session.id, 'add 6 7', BETAS);
    assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 2);
    assert.equal(fixtureA.calls.filter((call: any) => call.method === 'tools/call').length, 0);
    pass('R1 restart reacquires ownership and calls only exact active B');

    const beforeReplayIo = fixtureB.calls.length;
    const replay = await update(client, session.id, [serverB], 'persisted-swap-b');
    assert.equal(responseEtag(replay.response), replaceEtag, 'R2 returns the committed revision');
    assert.equal(fixtureB.calls.length, beforeReplayIo, 'R2 performs no realization I/O');
    pass('R2 idempotency receipt survives restart and replays without effect');

    const current = await retrieveSessionWithEtag(client, session.id, BETAS);
    const removed = await update(
      client,
      session.id,
      [],
      'persisted-remove-b',
      current.etag,
    );
    assert.deepEqual(removed.data.agent.mcp_servers, []);
    await hardKillServer(server, "MCP durable recovery");
    server = null;
    started = spawnServer('management', port, env);
    server = started.server;
    baseUrl = started.baseUrl;
    await waitForPort(port, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const afterRemovalRestart = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.deepEqual(afterRemovalRestart.agent.mcp_servers, [], 'R3 removed B stays removed');
    assert.equal(fixtureB.calls.filter((call: any) => call.method === 'tools/call').length, 2);
    pass('R3 restart never republishes a removed generation');

    // Cause/effect graph: desired count -> reachable fault points;
    // fault point -> durable active/ACK state at death;
    // SIGKILL -> loss of process-local projections -> new-process handshake;
    // durable desired generations + restart -> exact recovered active/ACK set;
    // persisted update identity + HTTP replay -> unchanged adapter I/O.
    // The constrained decision table below selects every reachable count/point
    // rule and checks both pre-crash and recovered effects for that same Session.
    // Extended cause/effect design: F1 stage complete before activation; F2
    // one of many publications applied; F3 an old generation drained; F4 all
    // publication effects applied before ACK; F5 ACK committed before response.
    // Cross F1..F5 with one/many attachments. Empty publication is reachable
    // only through retirement, so F3..F5 also run with zero desired attachments.
    // Each rule requires the real HTTP adapter and production driver, exact
    // crash marker, durable generation/ACK readback, restart and a terminal
    // observation. A dropped rule must fail the formal coverage manifest.
    const rules: Array<{ boundary: string; count: number }> = [];
    for (const count of [1, 2]) {
      for (const boundary of ['stage', 'publish-one', 'drain-one', 'published', 'ack-after']) rules.push({ boundary, count });
    }
    for (const boundary of ['drain-one', 'published', 'ack-after']) rules.push({ boundary, count: 0 });
    const chainTraces: any[] = [];
    const tracePath = path.join(observationDir, 'trace.ndjson');
    const markerPath = path.join(observationDir, 'boundary.json');
    const configPath = path.join(observationDir, 'armed.json');
    // J1 every incomplete final-record prefix contributes no observation;
    // J2 newline commits exactly one record; J3 malformed committed JSON fails.
    // This append protocol permits concurrent reads without hiding corruption.
    const parseCommittedTrace = (bytes: string): any[] =>
      bytes.slice(0, bytes.lastIndexOf('\n') + 1).split('\n').filter(Boolean).map(line => JSON.parse(line));
    const firstRecord = JSON.stringify({ sequence: 0, value: '中文' }) + '\n';
    const nextRecord = JSON.stringify({ sequence: 1, value: 'next' }) + '\n';
    for (let cut = 0; cut < nextRecord.length; cut++) {
      assert.deepEqual(parseCommittedTrace(firstRecord + nextRecord.slice(0, cut)), [JSON.parse(firstRecord)], `J1 prefix ${cut}`);
    }
    assert.equal(parseCommittedTrace(firstRecord + nextRecord).length, 2, 'J2 committed record');
    assert.throws(() => parseCommittedTrace(firstRecord + '{invalid}\n'), SyntaxError, 'J3 corruption');
    const readTrace = (): any[] => fs.existsSync(tracePath)
      ? parseCommittedTrace(fs.readFileSync(tracePath, 'utf8')) : [];
    const arm = (sessionId: string, boundary: string) => {
      fs.writeFileSync(`${configPath}.tmp`, JSON.stringify({ session_id: sessionId, boundary }));
      fs.renameSync(`${configPath}.tmp`, configPath);
    };
    for (const { boundary, count } of rules) {
      const rule = `${boundary}-${count}`;
      client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, maxRetries: 0 });
      // Isolate each rule's I/O oracle: recovery of earlier Sessions may run
      // concurrently, so shared global fixture counters cannot identify replay.
      const caseVault = await client.beta.vaults.create({ display_name: rule, betas: BETAS });
      const names = Array.from({ length: Math.max(count, 1) }, (_, index) => index === 0 ? 'calc' : `calc${index}`);
      const oldFixtures = await Promise.all(names.map(() => startCalcFixture(TOKEN_A)));
      const newFixtures = await Promise.all(names.map(() => startCalcFixture(TOKEN_B)));
      caseFixtures.push(...oldFixtures, ...newFixtures);
      for (const [fixtures, token] of [[oldFixtures, TOKEN_A], [newFixtures, TOKEN_B]] as const) {
        for (const fixture of fixtures) await client.beta.vaults.credentials.create(caseVault.id, { auth: { type: 'static_bearer', mcp_server_url: fixture.url, token }, betas: BETAS });
      }
      const originalServers: McpServer[] = names.map((name, index) => ({ name, type: 'url', url: oldFixtures[index].url }));
      const desired: McpServer[] = names.slice(0, count).map((name, index) => ({ name, type: 'url', url: newFixtures[index].url }));
      const candidate = await client.beta.sessions.create({
        agent: alwaysAllowMcpAgent('assistant', originalServers), environment_id: 'env_local', vault_ids: [caseVault.id], betas: BETAS,
      });
      fs.rmSync(tracePath, { force: true });
      fs.rmSync(markerPath, { force: true });
      arm(candidate.id, boundary);
      const pending = update(client, candidate.id, desired, `crash-${rule}`).then(() => 'response', () => 'interrupted');
      await waitForValue(() => fs.existsSync(markerPath), Boolean, `${rule}: exact production boundary`, { timeoutMs: 30_000 });
      const marker = JSON.parse(fs.readFileSync(markerPath, 'utf8'));
      assert.equal(marker.point, boundary);
      const initializationsBeforeCrash = newFixtures.map(
        (fixture: (typeof newFixtures)[number]) => fixture.calls.filter((call: any) => call.method === 'initialize').length,
      );
      await hardKillServer(server, `MCP exact boundary ${rule}`);
      server = null;
      assert.equal(await pending, 'interrupted', `${rule}: no response before crash`);
      const interrupted = readTrace();
      const lastRoot = interrupted.filter(event => event.data.attachments).at(-1).data;
      const active = lastRoot.attachments.filter((a: any) => a.state === 'active');
      if (boundary === 'ack-after') assert.ok(active.every((a: any) => a.ack), `${rule}: durable ACK`);
      else if (count > 0 && boundary !== 'stage') assert.ok(active.some((a: any) => !a.ack), `${rule}: ACK not yet committed`);
      fs.appendFileSync(tracePath, `${JSON.stringify({ sequence: interrupted.length, session: candidate.id, point: 'crash', data: {} })}\n`);
      arm(candidate.id, '');
      started = spawnServer('management', port, env);
      server = started.server;
      baseUrl = started.baseUrl;
      await waitForPort(port, 180_000, server);
      client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, maxRetries: 0 });
      const restored = await client.beta.sessions.retrieve(candidate.id, { betas: BETAS });
      assert.deepEqual(restored.agent.mcp_servers, desired, `${rule}: exact desired generation`);
      await waitForValue(
        readTrace,
        (events: ReturnType<typeof readTrace>) => events
          .slice(interrupted.length + 1)
          .some((event: any) => event.point === 'complete'),
        `${rule}: recovered terminal driver`,
        { timeoutMs: 30_000 },
      );
      // Every desired endpoint must perform a real authenticated handshake in
      // the new process, including the second attachment; zero desired means
      // no handshake. Driver observations alone cannot establish actual HTTP I/O.
      for (let index = 0; index < newFixtures.length; index++) {
        const actual = newFixtures[index].calls.filter((call: any) => call.method === 'initialize').length;
        assert.equal(actual, initializationsBeforeCrash[index] + (index < count ? 1 : 0), `${rule}: endpoint ${index} cold handshake`);
      }
      // Replay the persisted update identity: no new generation or adapter I/O.
      const callsBeforeReplay = newFixtures.reduce(
        (total: number, fixture: (typeof newFixtures)[number]) => total + fixture.calls.length,
        0,
      );
      await update(client, candidate.id, desired, `crash-${rule}`);
      assert.equal(
        newFixtures.reduce(
          (total: number, fixture: (typeof newFixtures)[number]) => total + fixture.calls.length,
          0,
        ),
        callsBeforeReplay,
        `${rule}: exact HTTP replay`,
      );
      const observed = readTrace();
      const terminalIndex = observed.findIndex((event, index) => index > interrupted.length && event.point === 'complete');
      const events = observed.slice(0, terminalIndex + 1);
      const finalRoot = events.filter(event => event.data.attachments).at(-1).data;
      const finalActive = finalRoot.attachments.filter((a: any) => a.state === 'active');
      assert.equal(finalRoot.created, lastRoot.created, `${rule}: creation observation retained`);
      assert.equal(finalActive.length, count, `${rule}: active cardinality`);
      assert.ok(finalActive.every((a: any) => a.ack), `${rule}: recovery ACK is durable`);
      if (count > 0) await sendManagedMessage(client, candidate.id, 'add 2 5', BETAS);
      chainTraces.push({ rule, cut: boundary, count, trace: events });
      fs.rmSync(configPath);
      pass(`exact MCP crash/recovery ${rule}`);
    }
    if (process.env.AWAKEN_OBSERVATION_TRACE_DIR) {
      fs.mkdirSync(process.env.AWAKEN_OBSERVATION_TRACE_DIR, { recursive: true });
      fs.writeFileSync(path.join(process.env.AWAKEN_OBSERVATION_TRACE_DIR, 'realization-http-crashes.json'), JSON.stringify({
        schema_version: 3, projection_id: 'session-observations-v3', name: 'realization-http-crashes', family: 'realization', steps: chainTraces,
      }, null, 2));
    }

    const serialized = JSON.stringify(afterRemovalRestart);
    assert.ok(!serialized.includes(TOKEN_A) && !serialized.includes(TOKEN_B));
    console.log('E2E PASS: ADR-0066 MCP recovery decision table.');
  } finally {
    const failedTrace = path.join(observationDir, 'trace.ndjson');
    if (process.env.AWAKEN_OBSERVATION_TRACE_DIR && fs.existsSync(failedTrace)) {
      fs.mkdirSync(process.env.AWAKEN_OBSERVATION_TRACE_DIR, { recursive: true });
      fs.copyFileSync(failedTrace, path.join(process.env.AWAKEN_OBSERVATION_TRACE_DIR, 'last-realization.ndjson'));
    }
    if (server) await stopServer(server);
    for (const fixture of caseFixtures) await fixture.close();
    await fixtureA.close();
    await fixtureB.close();
    upstream.close();
    fs.rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
