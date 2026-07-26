// Production composition E2E for the projected ACP launch on the local Workdir
// tier. A PATH-local `gemini` fixture speaks the real ACP JSON-RPC wire, while the
// aggregated `awaken` process performs the same catalog selection, exact credential
// admission, and sandbox realization used by an installed CLI. Gemini exercises
// process-secret projection; Codex proves a bearer-only publication cannot bypass
// its typed credential-artifact driver. No scenario-host composition is involved.
//
// Fixture launch cause graph: host OS -> native PATH delimiter + executable
// wrapper -> ACP JSON-RPC fixture starts -> durable run settles.
//
// | Rule | Host | PATH delimiter | Fixture wrapper | Result |
// |---|---|---|---|---|
// | L1 | Unix | `:` | `#!/bin/sh` | ACP turn |
// | L2 | Windows | `;` | native fixture `.exe` | ACP turn |

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

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
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    maxBuffer: 128 * 1024 * 1024,
  }).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch {
      // Cargo diagnostic.
    }
  }
  throw new Error('could not resolve the awaken binary');
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
  fs.writeFileSync(fixture, `#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}' ;;
    *'"method":"session/new"'*)
      printf '%s\\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"projected-session"}}' ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"projected-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"PROJECTED base=%s model=%s key=%s cwd=%s home=%s"}}}}\\n' \
        "$GOOGLE_GEMINI_BASE_URL" "$GEMINI_MODEL" "$(printf %s "$GEMINI_API_KEY" | cut -c1-6)" "$(pwd)" "$GEMINI_DIR"
      printf '%s\\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      exit 0 ;;
  esac
done
`);
  fs.chmodSync(fixture, 0o755);

}

function start(binary, cli) {
  const environment = { ...process.env };
  const configPath = path.join(TMP, 'config.toml');
  fs.writeFileSync(configPath, [
    `data_dir = ${JSON.stringify(STORAGE)}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    `control_seal_key = ${JSON.stringify(SEAL_KEY)}`,
    'sandbox_tier = "local"',
    `acp_clis = [${JSON.stringify(cli)}]`,
    `acp_default_cli = ${JSON.stringify(cli)}`,
  ].join('\n'));
  return spawn(binary, ['serve', '--config', configPath], {
    env: {
      ...environment,
      PATH: `${BIN_DIR}${path.delimiter}${environment.PATH ?? ''}`,
      // These ambient values are deliberately wrong. The launched CLI must receive
      // only the endpoint, upstream model, and credential revision published below.
      GOOGLE_GEMINI_BASE_URL: 'http://ambient-gemini.invalid/v1',
      GEMINI_API_KEY: 'ambient-gemini-must-not-win', // awaken-allow: secret (fixture)
      GEMINI_MODEL: 'environment-fallback-must-not-win',
    },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
}

async function ready(child) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ host: '127.0.0.1', port: PORT });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    if (child.exitCode !== null) throw new Error(`awaken exited with ${child.exitCode}`);
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

async function messages(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  const texts = events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '');
  const failures = events.filter((event) => event.type === 'session.error');
  return [...texts, ...failures.map((event) => `ERROR:${JSON.stringify(event.error)}`)];
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
    agent, provider, endpoint, model, upstreamModel, baseUrl, secret, envKey,
  } = definition;
  await request(base, 'PUT', `/v1/config/providers/${provider}`, {
    id: provider,
    slug: provider,
    display_name: provider,
    version: 1,
  });
  await request(base, 'PUT', `/v1/config/endpoints/${endpoint}`, {
    id: endpoint,
    provider_id: provider,
    dialect: 'open_ai_chat',
    base_url: baseUrl,
    timeout_secs: 30,
    display_name: endpoint,
    version: 1,
  });
  await request(base, 'POST', '/v1/config/offerings', {
    model_id: model,
    provider_id: provider,
    protocol_endpoint_id: endpoint,
    dialect: 'open_ai_chat',
    upstream_model: upstreamModel,
  });
  await request(base, 'POST', '/v1/config/credentials', {
    workspace_id: WORKSPACE,
    kind: 'vault',
    provider_id: provider,
    env_key: envKey,
    secret,
  });
  await request(base, 'PUT', `/v1/config/agents/${agent}`, {
    name: agent,
    model: { id: model, provider_identity_ref: provider },
    system: 'Exercise publication-pinned ACP provisioning.',
    tools: [],
  });
  await request(base, 'POST', `/v1/config/agents/${agent}/publish`);
}

async function main() {
  installGeminiFixture();
  fs.mkdirSync(STORAGE, { recursive: true });
  const binary = awakenBin();
  let server = start(binary, 'gemini');
  const mcpToken = 'projected-codex-mcp-token'; // awaken-allow: secret (fixture)
  const fixture = await startCalcFixture(mcpToken);
  const anonymousFixture = await startCalcFixture('unused-anonymous-token', {
    allowAnonymous: true,
  });
  try {
    await ready(server);
    WORKSPACE = fs.readFileSync(path.join(STORAGE, 'platform-workspace-id'), 'utf8').trim();
    const base = `http://127.0.0.1:${PORT}`;
    await publishProviderAgent(base, {
      agent: GEMINI_AGENT,
      provider: 'google',
      endpoint: 'gemini-endpoint',
      model: 'gemini-published',
      upstreamModel: 'gemini-upstream',
      baseUrl: 'http://gemini-db.invalid/v1',
      secret: 'persisted-gemini-key', // awaken-allow: secret (fixture)
      envKey: 'GEMINI_API_KEY',
    });
    await publishProviderAgent(base, {
      agent: CODEX_AGENT,
      provider: 'openai',
      endpoint: 'codex-endpoint',
      model: 'codex-published',
      upstreamModel: 'codex-upstream',
      baseUrl: 'http://codex-db.invalid/v1',
      secret: 'persisted-codex-key', // awaken-allow: secret (fixture)
      envKey: 'OPENAI_API_KEY',
    });
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const session = await client.beta.sessions.create({
      agent: GEMINI_AGENT,
      metadata: { 'awaken.runtime': 'acp:gemini' },
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise projection' }] }],
      betas: BETAS,
    });
    const geminiTexts = await messages(client, session.id);
    const reply = geminiTexts.find((text) => text.includes('PROJECTED'));
    assert.ok(reply, `the production projected ACP process returned an agent message: ${JSON.stringify(geminiTexts)}`);
    assert.match(reply, /base=http:\/\/gemini-db\.invalid\/v1/u);
    assert.match(reply, /model=gemini-upstream/u);
    assert.match(reply, /key=persis/u);
    assert.ok(!reply.includes('ambient-gemini'));
    assert.ok(!reply.includes('environment-fallback-must-not-win'));
    assert.match(reply, /cwd=.*awaken-acp-sbx/u);
    assert.match(reply, /home=.*awaken-acp-sbx/u);
    assert.ok(!reply.includes(os.homedir()), 'the CLI never receives the operator home');

    await stop(server);
    server = start(binary, 'codex');
    await ready(server);
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
    // Cause/effect graph for MCP on the Workdir provider:
    // C1=credential is selected; C2=provider proves substitution + no bypass.
    // C1 + !C2 -> M1 reject Worker custody before launch.
    // !C1       -> M2 project the anonymous endpoint into the same ACP config path.
    //
    // | Rule | credential | provider proof | result                    |
    // | M1   | yes        | no             | fail closed               |
    // | M2   | no         | n/a            | config mounted and launch |
    await assert.rejects(
      codexClient.beta.sessions.create({
        agent: CODEX_AGENT,
        metadata: { 'awaken.runtime': 'acp:codex' },
        mcp_servers: [{ name: 'calc-secure', type: 'url', url: fixture.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      }),
      (error) => error?.status === 500 && String(error).includes('provider-enforced secret substitution'),
      'M1: Workdir must not silently downgrade authenticated MCP out of Worker custody',
    );
    const codexSession = await codexClient.beta.sessions.create({
      agent: CODEX_AGENT,
      metadata: { 'awaken.runtime': 'acp:codex' },
      mcp_servers: [{ name: 'calc-anonymous', type: 'url', url: anonymousFixture.url }],
      betas: BETAS,
    });
    await codexClient.beta.sessions.events.send(codexSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise config mount' }] }],
      betas: BETAS,
    });
    const codexTexts = await messages(codexClient, codexSession.id);
    assert.ok(
      codexTexts.some((text) => text.includes('credential_driver_required: codex')),
      `Codex rejects bearer-only publication instead of restoring its removed environment protocol: ${JSON.stringify(codexTexts)}`,
    );

    console.log('E2E PASS: aggregated awaken projects publication-pinned Gemini access and rejects bearer-only Codex access before launch; authenticated MCP remains fail closed on Workdir.');
  } finally {
    await stop(server).catch(() => {});
    await fixture.close();
    await anonymousFixture.close();
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
