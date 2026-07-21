// Production composition E2E for the projected ACP launch on the local Workdir
// tier. A PATH-local `gemini` fixture speaks the real ACP JSON-RPC wire, while the
// aggregated `awaken` process performs the same catalog selection, model endpoint /
// credential injection, per-thread config-home projection, and sandbox realization
// used by an installed CLI. No scenario-host-only composition is involved.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38442);
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-projected-local-'));
const STORAGE = path.join(TMP, 'storage');
const BIN_DIR = path.join(TMP, 'bin');
const BETAS = ['managed-agents-2026-04-01'];

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

  const npx = path.join(BIN_DIR, 'npx');
  fs.writeFileSync(npx, `#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}' ;;
    *'"method":"session/new"'*)
      printf '%s\\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"codex-session"}}' ;;
    *'"method":"session/prompt"'*)
      mounted=no; test -f "$CODEX_HOME/config.toml" && mounted=yes
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"codex-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CODEX_PROJECTED base=%s key=%s home=%s config=%s"}}}}\\n' \
        "$OPENAI_BASE_URL" "$(printf %s "$OPENAI_API_KEY" | cut -c1-6)" "$CODEX_HOME" "$mounted"
      printf '%s\\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      exit 0 ;;
  esac
done
`);
  fs.chmodSync(npx, 0o755);
}

function start(binary, cli) {
  const environment = { ...process.env };
  for (const key of ['AWAKEN_ACP_ARGV', 'AWAKEN_ACP_CREDENTIAL_FILE']) delete environment[key];
  return spawn(binary, {
    env: {
      ...environment,
      PATH: `${BIN_DIR}:${environment.PATH ?? ''}`,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_STORAGE_DIR: STORAGE,
      AWAKEN_ACP_CLI: cli,
      AWAKEN_SANDBOX_TIER: 'local',
      GOOGLE_GEMINI_BASE_URL: 'http://model-gateway.invalid/v1',
      GEMINI_API_KEY: 'lease-projected-e2e', // awaken-allow: secret (fixture)
      GEMINI_MODEL: 'environment-fallback-must-not-win',
      OPENAI_BASE_URL: 'http://codex-gateway.invalid/v1',
      OPENAI_API_KEY: 'lease-codex-e2e', // awaken-allow: secret (fixture)
      OPENAI_MODEL: 'codex-fallback-must-not-win',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
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
  return events
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '');
}

async function main() {
  installGeminiFixture();
  fs.mkdirSync(STORAGE, { recursive: true });
  const binary = awakenBin();
  let server = start(binary, 'gemini');
  const mcpToken = 'projected-codex-mcp-token'; // awaken-allow: secret (fixture)
  const fixture = await startCalcFixture(mcpToken);
  try {
    await ready(server);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const environment = await client.beta.environments.create({
      name: `projected-local-${process.pid}`,
      config: {
        type: 'cloud',
        networking: { type: 'unrestricted' },
        sandbox: { isolation: 'workdir', network: { mode: 'unrestricted' } },
      },
      betas: BETAS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: environment.id,
      metadata: { 'awaken.runtime': 'acp:gemini' },
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise projection' }] }],
      betas: BETAS,
    });
    const reply = (await messages(client, session.id)).find((text) => text.includes('PROJECTED'));
    assert.ok(reply, 'the production projected ACP process returned an agent message');
    assert.match(reply, /base=http:\/\/model-gateway\.invalid\/v1/u);
    assert.match(reply, /model=unconfigured/u, 'the frozen run model wins over the fallback model env');
    assert.ok(!reply.includes('environment-fallback-must-not-win'));
    assert.match(reply, /key=lease-/u);
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
    const codexEnvironment = await codexClient.beta.environments.create({
      name: `projected-codex-${process.pid}`,
      config: { type: 'cloud', networking: { type: 'unrestricted' } },
      betas: BETAS,
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
    const codexSession = await codexClient.beta.sessions.create({
      agent: 'assistant',
      environment_id: codexEnvironment.id,
      metadata: { 'awaken.runtime': 'acp:codex' },
      mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
      vault_ids: [vault.id],
      betas: BETAS,
    });
    await codexClient.beta.sessions.events.send(codexSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise config mount' }] }],
      betas: BETAS,
    });
    const codexReply = (await messages(codexClient, codexSession.id))
      .find((text) => text.includes('CODEX_PROJECTED'));
    assert.ok(codexReply, 'the projected Codex adapter returned an agent message');
    assert.match(codexReply, /base=http:\/\/codex-gateway\.invalid\/v1/u);
    assert.match(codexReply, /key=lease-/u);
    assert.match(codexReply, /home=\.acp-config/u);
    assert.match(codexReply, /config=yes/u, 'the projected config.toml was materialized');

    console.log('E2E PASS: aggregated awaken projects Gemini and Codex launches into per-thread local sandboxes, including config-file materialization.');
  } finally {
    await stop(server).catch(() => {});
    await fixture.close();
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
