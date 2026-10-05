// Production composition E2E for the projected ACP launch on the local Namespace
// tier. A PATH-local `gemini` fixture speaks the real ACP JSON-RPC wire, while the
// aggregated `awaken` process performs the same catalog selection, exact credential
// admission, and sandbox realization used by an installed CLI. Gemini exercises
// process-secret projection; Codex exercises its typed provider artifact and
// process-private MCP credential route. No scenario-host composition is involved.
//
// Fixture launch cause graph: host OS -> native PATH delimiter + executable
// wrapper -> ACP JSON-RPC fixture starts -> durable run settles.
//
// | Rule | Host | PATH delimiter | Fixture wrapper | Result |
// |---|---|---|---|---|
// | L1 | Unix | `:` | `#!/usr/bin/env node` | ACP turn |
// | L2 | Windows | `;` | native fixture `.exe` | ACP turn |
//
// Isolation/credential cause graph: C1=Provider provisioning injects a credential
// into an opaque ACP process; C2=Namespace is available; C3=the exact ACP adapter
// supports the published realization kind. C1 requires a tool-transparent,
// path-faithful boundary, and C3 must be proven before process launch.
//
// | Rule | provisioning | configured tier | effect |
// |---|---|---|---|
// | P1 | Provider + supported adapter | Namespace | launch with exact endpoint/model/secret |
// | P2 | Provider + supported adapter | Workdir | no eligible Worker; never launch |
// | P3 | Provider + supported MCP credential | Namespace | exact process-private MCP route |
//
// This scenario owns P1 and P3. The shared placement-kernel tests own P2; the
// container scenario owns unsupported credential custody before launch.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import { closeHttpServer } from './http_server.mjs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { waitForVerifiedAcpCapability } from './fixtures/acp_capability.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import { initializeE2EInstallation, waitForSessionEventReceipt } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38442);
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-projected-local-'));
const STORAGE = path.join(TMP, 'storage');
const BIN_DIR = path.join(TMP, 'bin');
const BETAS = ['managed-agents-2026-04-01'];
let WORKSPACE;
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const GEMINI_AGENT = 'projected-gemini-agent';
const CODEX_AGENT = 'projected-codex-agent';

function awakenBin() {
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  });
}

function installGeminiFixture() {
  fs.mkdirSync(BIN_DIR, { recursive: true });
  if (process.platform === 'win32') {
    const gemini = path.join(BIN_DIR, 'gemini.exe');
    execFileSync('rustc', [
      '--edition=2024',
      path.join(ROOT, 'e2e', 'fixtures', 'acp_projected_windows_fixture.rs'),
      '-o', gemini,
    ]);
    fs.copyFileSync(gemini, path.join(BIN_DIR, 'npx.exe'));
    return;
  }
  const fixture = path.join(BIN_DIR, 'gemini');
  // The launch sandbox owns its projected PATH. A shebang that embeds this
  // test runner's private NVM path is not an installed ACP runtime dependency.
  fs.writeFileSync(fixture, `#!/usr/bin/env node
if (process.argv.includes('--version')) {
  console.log('gemini-fixture 1.0.0');
  process.exit(0);
}
if (process.argv.includes('--list-sessions')) process.exit(0);
const readline = require('node:readline');
const lines = readline.createInterface({ input: process.stdin });
lines.on('line', (line) => {
  const request = JSON.parse(line);
  if (request.method === 'initialize') {
    console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: 1,
      agentCapabilities: { loadSession: true, promptCapabilities: { embeddedContext: true },
        mcpCapabilities: { http: true } } } }));
  } else if (request.method === 'session/new') {
    console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: {
      sessionId: 'projected-session', modes: { currentModeId: 'code',
        availableModes: [{ id: 'code', name: 'Code' }] }, configOptions: [],
    } }));
  } else if (request.method === 'session/prompt') {
    const text = ['PROJECTED', 'base=' + process.env.GOOGLE_GEMINI_BASE_URL,
      'model=' + process.env.GEMINI_MODEL, 'key=' + (process.env.GEMINI_API_KEY || '').slice(0, 6),
      'cwd=' + process.cwd(), 'home=' + process.env.GEMINI_CLI_HOME].join(' ');
    console.log(JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: {
      sessionId: 'projected-session', update: { sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text } },
    } }));
    console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }));
    process.exit(0);
  }
});
`);
  fs.chmodSync(fixture, 0o755);

}

function installCodexAdapterFixture() {
  const prefix = path.join(STORAGE, 'acp-wrappers', 'codex');
  const executable = path.join(prefix, 'node_modules', '.bin', 'codex-acp');
  fs.mkdirSync(path.dirname(executable), { recursive: true });
  fs.writeFileSync(path.join(prefix, 'package.json'), JSON.stringify({
    dependencies: { '@agentclientprotocol/codex-acp': '1.10.0' },
  }));
  fs.writeFileSync(executable, `#!/usr/bin/env node
const readline = require('node:readline');
const lines = readline.createInterface({ input: process.stdin });
let mcpServers = [];
lines.on('line', async (line) => {
  const request = JSON.parse(line);
  if (request.method === 'initialize') {
    console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { protocolVersion: 1,
      agentCapabilities: { loadSession: true, promptCapabilities: { embeddedContext: true },
        mcpCapabilities: { http: true } } } }));
  } else if (request.method === 'session/new' || request.method === 'session/load') {
    mcpServers = request.params.mcpServers || [];
    console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: {
      sessionId: 'projected-codex-session', modes: { currentModeId: 'code',
        availableModes: [{ id: 'code', name: 'Code' }] }, configOptions: [],
    } }));
  } else if (request.method === 'session/prompt') {
    const server = mcpServers.find((entry) => entry.name === 'calc-secure');
    if (!server) process.exit(1);
    try {
      const headers = Object.fromEntries((server.headers || []).map((entry) => [entry.name, entry.value]));
      const response = await fetch(server.url, { method: 'POST', headers: {
        ...headers, 'content-type': 'application/json',
      }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'tools/call',
        params: { name: 'add', arguments: { a: 2, b: 3 } } }) });
      if (!response.ok) process.exit(1);
      const result = await response.json();
      const text = 'SECURE_MCP=' + result.result.content[0].text;
      console.log(JSON.stringify({ jsonrpc: '2.0', method: 'session/update', params: {
        sessionId: 'projected-codex-session', update: { sessionUpdate: 'agent_message_chunk',
          content: { type: 'text', text } },
      } }));
      console.log(JSON.stringify({ jsonrpc: '2.0', id: request.id, result: { stopReason: 'end_turn' } }));
      process.exit(0);
    } catch {
      process.exit(1);
    }
  }
});
`);
  fs.chmodSync(executable, 0o755);
}

function start(binary, cli) {
  const environment = { ...process.env };
  const configPath = path.join(TMP, 'config.toml');
  fs.writeFileSync(configPath, [
    `data_dir = ${JSON.stringify(STORAGE)}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    `control_seal_key = ${JSON.stringify(SEAL_KEY)}`,
    // Cause/effect rule A1: this scenario owns ACP capability/projection, not
    // IAM. Explicit no-login keeps capability polling and config authoring in
    // that scope; embedded-IAM denial is covered by management_authz_e2e.
    'identity_mode = "no-login"',
    // P1: projected credentials for an arbitrary ACP process require the local
    // OS-isolated provider. Workdir is intentionally ineligible for this run.
    'sandbox_tier = "namespace"',
    // The projection contract is portable across CI hosts where unprivileged
    // user namespaces are disabled. The default remains fail-closed; this
    // scenario explicitly opts into the documented local fallback while the
    // Docker E2E owns the isolation assertion.
    'sandbox_allow_local_fallback = true',
    `acp_clis = [${JSON.stringify(cli)}]`,
  ].join('\n'));
  const childEnvironment = {
    ...environment,
    PATH: `${BIN_DIR}${path.delimiter}${environment.PATH ?? ''}`,
    // These non-secret ambient values are deliberately wrong. Provider keys
    // are absent because production rejects them at startup. The launched CLI
    // must receive only the endpoint, model, and credential published below.
    GOOGLE_GEMINI_BASE_URL: 'http://ambient-gemini.invalid/v1',
    GEMINI_MODEL: 'environment-fallback-must-not-win',
  };
  initializeE2EInstallation(childEnvironment, { binary, configPath });
  return spawn(binary, automatedAllInOneArgs('--config', configPath), {
    env: childEnvironment,
    stdio: ['ignore', 'inherit', 'inherit'],
  });
}

async function ready(child) {
  // Readiness lifecycle cause/effect graph: C1=TCP listener accepts; C2=child
  // exits normally; C3=child exits by signal. Effects: E1=ready; E2/E3=fail
  // immediately with the exact terminal cause; otherwise retry until deadline.
  //
  // | Rule | C1 | C2 | C3 | effect |
  // | R1 | yes | no | no | ready |
  // | R2 | no | yes | no | report exit code |
  // | R3 | no | no | yes | report signal |
  // | R4 | no | no | no | retry, then timeout |
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ host: '127.0.0.1', port: PORT });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(
        child.signalCode === null
          ? `awaken exited with ${child.exitCode}`
          : `awaken exited from signal ${child.signalCode}`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error('awaken did not become ready');
}

async function stop(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once('exit', resolve));
  child.kill('SIGINT');
  await exited;
}

async function listEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

function projectedMessages(events) {
  const texts = events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '');
  const failures = events.filter((event) => event.type === 'session.error');
  return [...texts, ...failures.map((event) => `ERROR:${JSON.stringify(event.error)}`)];
}

function agentWithMcpServer(id, server) {
  return {
    id,
    type: 'agent_with_overrides',
    mcp_servers: [server],
    tools: [{
      type: 'mcp_toolset',
      mcp_server_name: server.name,
      default_config: {
        enabled: true,
        permission_policy: { type: 'always_allow' },
      },
    }],
  };
}

async function request(base, method, route, body) {
  const response = await fetch(`${base}${route}`, {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  assert.ok(
    response.ok,
    `${method} ${route}: ${response.status} ${JSON.stringify(value)}`,
  );
  return value;
}

async function publishProviderAgent(base, definition) {
  const {
    agent, backend, provider, model, baseUrl, secret, dialect,
  } = definition;
  await request(base, 'POST', '/v1/config/provider-connections', {
    idempotency_key: `acp-projected-local-${agent}`,
    workspace_id: WORKSPACE,
    provider_id: provider,
    display_name: provider,
    dialect,
    base_url: baseUrl,
    timeout_secs: 30,
    secret,
  });
  await request(base, 'PUT', `/v1/config/agents/${agent}`, {
    name: agent,
    // One Managed model-id codec owns provider-routed ACP intent. Publication
    // resolves this Target to the canonical WorkerLocal execution identity plus
    // the selected provider endpoint; an ad-hoc pinned object would conflate them.
    model: `${model};provider=${provider};api=${dialect};executor=${backend}`,
    system: 'Exercise publication-pinned ACP provisioning.',
    tools: [],
  });
  await request(base, 'POST', `/v1/config/agents/${agent}/publish`);
}

async function startModelDirectory() {
  const server = http.createServer((request, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    if (request.url?.startsWith('/gemini/')) {
      response.end(JSON.stringify({ models: [{ name: 'models/gemini-upstream' }] }));
    } else {
      response.end(JSON.stringify({ data: [{ id: 'codex-upstream' }] }));
    }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  return {
    url: `http://127.0.0.1:${address.port}`,
    close: () => closeHttpServer(server),
  };
}

async function main() {
  // Test design (projected local ACP arms). Causes: C1=the catalog verifies the
  // projected Gemini executable; C2=the publication pins provider/model/MCP;
  // C3=the server restarts over the same durable store; C4=the ACP exits cleanly
  // or by signal. Effects: E1=C1+C2 runs the exact JSON-RPC lifecycle and tool;
  // E2=C3 reloads the existing ACP Session and history; E3=C4 maps process exit
  // to the classified terminal boundary without a phantom success.
  // Constraints/invariant: catalog identity, publication pin, and durable ACP
  // session id are single authorities. C5=the Coordinator owns projection
  // policy without an ACP executor while its registered Worker owns the exact
  // launch route; E4=C5 permits the committed Session input to reach that
  // Worker without installing a second Coordinator executor. C6=a Direct host
  // executable is discovered on the fixture PATH; E5=preparation pins its
  // absolute argv so a restricted Worker PATH cannot replace or lose it.
  // C7=the Unix fixture uses the sandbox's `env node` interpreter instead of
  // the outer test runner's private Node path; E6=both Gemini and Codex enter
  // their real ACP handshake inside the isolated runtime.
  // Decision rules: L1=C1+C2=>E1; L2=L1+C3=>E2; L3=C4=>E3;
  // L4=C1+C2+C5=>E4; L5=C1+C6+C7=>E5+E6 and the real ACP reply.
  installGeminiFixture();
  fs.mkdirSync(STORAGE, { recursive: true });
  const binary = awakenBin();
  let server = start(binary, 'gemini');
  const mcpToken = 'projected-codex-mcp-token'; // awaken-allow: secret (fixture)
  const fixture = await startCalcFixture(mcpToken);
  const directory = await startModelDirectory();
  try {
    await ready(server);
    await waitForVerifiedAcpCapability(`http://127.0.0.1:${PORT}`, 'gemini');
    WORKSPACE = fs.readFileSync(path.join(STORAGE, 'platform-workspace-id'), 'utf8').trim();
    const base = `http://127.0.0.1:${PORT}`;
    await publishProviderAgent(base, {
      agent: GEMINI_AGENT,
      backend: 'acp:gemini',
      provider: 'gemini',
      model: 'gemini-upstream',
      dialect: 'gemini',
      baseUrl: `${directory.url}/gemini/v1beta/`,
      secret: 'persisted-gemini-key', // awaken-allow: secret (fixture)
    });
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const session = await client.beta.sessions.create({
      agent: GEMINI_AGENT,
      environment_id: 'env_local',
      betas: BETAS,
    });
    // P1 receipt rule: C4 exact projected command receipt; C5 its PROJECTED
    // reply; C6 the same Session's terminal end_turn. E4 the processed receipt,
    // reply and complete terminal precede server restart. K1 old history cannot
    // satisfy the receipt delta. Rule C4+C5+C6=>E4; C4+C5 without C6 remains
    // active, so stopping the server there could strand this Gemini Run while
    // the next phase only serves Codex.
    const projectedReceipt = (await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise projection' }] }],
      betas: BETAS,
    })).data[0];
    const projectedObservation = await waitForSessionEventReceipt(
      client,
      session.id,
      projectedReceipt.id,
      BETAS,
      ({ delta }) => projectedMessages(delta).some((text) => text.includes('PROJECTED'))
        && delta.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
      'projected local Gemini response',
      { timeoutMs: 60_000 },
    );
    const geminiTexts = projectedMessages(projectedObservation.delta);
    const reply = geminiTexts.find((text) => text.includes('PROJECTED'));
    assert.ok(reply, `the production projected ACP process returned an agent message: ${JSON.stringify(geminiTexts)}`);
    assert.match(reply, new RegExp(`base=${directory.url.replaceAll(".", "\\.")}/gemini/v1beta/`, "u"));
    assert.match(reply, /model=gemini-upstream/u);
    assert.match(reply, /key=persis/u);
    assert.ok(!reply.includes('ambient-gemini'));
    assert.ok(!reply.includes('environment-fallback-must-not-win'));
    const paths = /cwd=([^ ]+) home=([^ ]+)/u.exec(reply);
    assert.ok(paths, `ACP response must expose cwd and config home: ${reply}`);
    assert.ok(
      paths[1] === '/workspace' || paths[1].startsWith(`${STORAGE}/sandboxes/scope-`),
      `cwd must be the isolated workspace or explicit local-fallback scope: ${paths[1]}`,
    );
    assert.equal(paths[2], `${paths[1]}/.acp-config`);
    assert.ok(!reply.includes(os.homedir()), 'the CLI never receives the operator home');

    await stop(server);
    // Pre-provision the exact pinned adapter revision so this deterministic
    // scenario never depends on npm/network or the operator's Codex adapter.
    // Host discovery still uses the installed Codex CLI identity, while the
    // fixture owns only the ACP protocol boundary exercised below.
    installCodexAdapterFixture();
    server = start(binary, 'codex');
    await ready(server);
    await waitForVerifiedAcpCapability(`http://127.0.0.1:${PORT}`, 'codex');
    // Publication capability rule: only the currently registered, freshly
    // negotiated exact ACP backend may be frozen. Publishing Codex while the
    // prior Gemini incarnation was live would correctly fail closed.
    await publishProviderAgent(base, {
      agent: CODEX_AGENT,
      backend: 'acp:codex',
      provider: 'openai',
      model: 'codex-upstream',
      // The canonical Codex ACP catalog row consumes the Responses dialect.
      // Publication must use that same capability fact; using Chat here would
      // correctly fail at admission and never reach this scenario's intended
      // credential-artifact rejection boundary (P3).
      dialect: 'open_ai_responses',
      baseUrl: `${directory.url}/openai/v1/`,
      secret: 'persisted-codex-key', // awaken-allow: secret (fixture)
    });
    const codexClient = new Anthropic({
      apiKey: 'e2e-dummy',
      baseURL: `http://127.0.0.1:${PORT}`,
    });
    const vault = await codexClient.beta.vaults.create({
      display_name: 'Projected Codex MCP vault',
      betas: BETAS,
    });
    await codexClient.beta.vaults.credentials.create(vault.id, {
      type: 'mcp_oauth',
      mcp_server_url: fixture.url,
      access_token: mcpToken,
      betas: BETAS,
    });
    // Local MCP cause/effect table: C1=the exact vault credential is selected;
    // C2=Codex declares HTTP client injection; C3=a command drives the Run.
    // M1 C1+C2+C3 => the process-private ACP route makes one authenticated MCP
    // call, the exact receipt is processed, and the Session returns idle.
    // M2 !C1+C3 => the separate fixture's provider failure reports a Run error
    // while its Session stays reusable and makes no additional MCP call.
    // The container D1 rule owns unsupported credential custody and its
    // before-launch terminal outcome; repeating it here would contradict the
    // Codex catalog capability and this local Worker's installed mechanism.
    const secureSession = await codexClient.beta.sessions.create({
      agent: agentWithMcpServer(CODEX_AGENT, {
        name: 'calc-secure', type: 'url', url: fixture.url,
      }),
      environment_id: 'env_local',
      vault_ids: [vault.id],
      betas: BETAS,
    });
    const secureReceipt = await codexClient.beta.sessions.events.send(secureSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'call the secure MCP route' }] }],
      betas: BETAS,
    });
    const acceptedSecure = secureReceipt.data[0];
    assert.equal(acceptedSecure?.type, 'user.message', 'M1 exact User Event receipt family');
    assert.equal(acceptedSecure?.processed_at, null, 'M1 receipt precedes execution');
    const secureObservation = await waitForSessionEventReceipt(
      codexClient,
      secureSession.id,
      acceptedSecure.id,
      BETAS,
      ({ delta }) => projectedMessages(delta).some((text) => text.includes('SECURE_MCP=5'))
        && delta.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
      'the process-private Codex MCP call and its completed Run',
      { timeoutMs: 60_000 },
    );
    const secureEvents = secureObservation.events;
    const retainedSecure = secureObservation.receiptEvent;
    assert.equal(
      typeof retainedSecure?.processed_at,
      'string',
      'M1 durable history anchors the exact command to its successful Run terminal',
    );
    assert.equal(secureEvents.filter((event) => event.type === 'session.error').length, 0, 'M1');
    assert.ok(projectedMessages(secureObservation.delta).some((text) => text.includes('SECURE_MCP=5')), 'M1');
    assert.equal(
      (await codexClient.beta.sessions.retrieve(secureSession.id, { betas: BETAS })).status,
      'idle',
      'M1 authenticated MCP call completes the Session',
    );
    assert.ok(fixture.calls.some((call) => call.method === 'tools/call'
      && call.authorization === `Bearer ${mcpToken}`), 'M1 exact vault bearer reached only the MCP server');
    assert.equal(fixture.unauthorized, 0, 'M1 no unauthenticated MCP request');
    const secureMcpCalls = fixture.calls.length;
    const codexSession = await codexClient.beta.sessions.create({
      agent: agentWithMcpServer(CODEX_AGENT, {
        name: 'calc-uncredentialed', type: 'url', url: fixture.url,
      }),
      environment_id: 'env_local',
      betas: BETAS,
    });
    // M2 receipt rule: C1 provider execution is rejected after realization and
    // C2 the exact command receipt exists; E1 its processed delta exposes one
    // public diagnostic while Session remains reusable. K1 excludes M1/older
    // errors. Decision M2=C1+C2=>E1.
    const codexReceipt = (await codexClient.beta.sessions.events.send(codexSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise provider rejection' }] }],
      betas: BETAS,
    })).data[0];
    const codexObservation = await waitForSessionEventReceipt(
      codexClient,
      codexSession.id,
      codexReceipt.id,
      BETAS,
      ({ delta }) => projectedMessages(delta).some((text) => (
        text.startsWith('ERROR:') || text.includes('stream disconnected before completion')
      )),
      'Codex provider/CLI failure diagnostic',
      { timeoutMs: 60_000 },
    );
    const codexTexts = projectedMessages(codexObservation.delta);
    assert.ok(
      codexTexts.some((text) => (
        text.startsWith('ERROR:') || text.includes('stream disconnected before completion')
      )),
      `M2: failed Run must expose a diagnostic without terminalizing its reusable Session: ${JSON.stringify(codexTexts)}`,
    );
    const providerSession = await codexClient.beta.sessions.retrieve(codexSession.id, { betas: BETAS });
    assert.equal(providerSession.status, 'idle', 'M2: execution failure does not masquerade as realization failure');
    assert.equal(fixture.calls.length, secureMcpCalls, 'M2: provider rejection performs no additional MCP I/O');

    console.log('E2E PASS: aggregated awaken projects publication-pinned Gemini access, completes authenticated Codex MCP work, and preserves independent Codex failure diagnostics.');
  } finally {
    await directory.close();
    await stop(server).catch(() => {});
    await fixture.close();
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
