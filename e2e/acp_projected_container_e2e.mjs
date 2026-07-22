// Production composition E2E for a config-plane-selected ACP CLI in the Docker
// sandbox tier. Unlike managed_container_agent_e2e.mjs (the fixed newline-wire
// seam), this starts the aggregated `awaken` binary and proves that one resolved
// run projects its model access, ACP MCP delivery, and File binding into the same
// process-as-container launch.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38513);
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-projected-container-'));
const STORAGE = path.join(TMP, 'storage');
const IMAGE = `awaken-acp-projected-e2e:${process.pid}`;
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MCP_TOKEN = 'projected-container-mcp-token'; // awaken-allow: secret (fixture)

function dockerAvailable() {
  return spawnSync('docker', ['version'], { stdio: 'ignore' }).status === 0;
}

function buildFixtureImage() {
  const context = path.join(TMP, 'image');
  fs.mkdirSync(context, { recursive: true });
  fs.writeFileSync(path.join(context, 'Dockerfile'), `FROM busybox:1.36
COPY bridge /usr/local/bin/bridge
COPY gemini /usr/local/bin/gemini
RUN chmod 0555 /usr/local/bin/bridge /usr/local/bin/gemini && mkdir -p /workspace
ENTRYPOINT ["/usr/local/bin/bridge"]
`);
  fs.writeFileSync(path.join(context, 'bridge'), `#!/bin/sh
set -eu
test "$1" = gemini
test "$2" = --experimental-acp
exec nc -lk -p 8080 -e /usr/local/bin/gemini
`);
  fs.writeFileSync(path.join(context, 'gemini'), `#!/bin/sh
set -eu
mcp=no
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}' ;;
    *'"method":"session/new"'*)
      case "$line" in *container-fixture*) mcp=yes ;; esac
      printf '%s\\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"container-projected-session"}}' ;;
    *'"method":"session/prompt"'*)
      mounted=no
      test "$(cat /workspace/.mnt/workspace/container-input.txt 2>/dev/null || true)" = CONTAINER_INPUT_OK && mounted=yes
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"container-projected-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"CONTAINER_PROJECTED base=%s model=%s key=%s file=%s mcp=%s home=%s"}}}}\\n' \
        "$GOOGLE_GEMINI_BASE_URL" "$GEMINI_MODEL" "$(printf %s "$GEMINI_API_KEY" | cut -c1-6)" "$mounted" "$mcp" "$GEMINI_DIR"
      printf '%s\\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
      exit 0 ;;
  esac
done
`);
  execFileSync('docker', ['build', '--quiet', '--tag', IMAGE, context], {
    cwd: ROOT,
    stdio: ['ignore', 'ignore', 'inherit'],
  });
}

function awakenBin() {
  const output = execFileSync(
    'cargo',
    [
      'build',
      '--quiet',
      '--message-format=json',
      '-p',
      'awaken-cli',
      '--bin',
      'awaken',
      '--features',
      'container-docker',
    ],
    { cwd: ROOT, maxBuffer: 128 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch {
      // Cargo diagnostic.
    }
  }
  throw new Error('could not resolve the container-enabled awaken binary');
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
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
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
  return { events, texts };
}

async function main() {
  if (!dockerAvailable()) {
    console.log('E2E SKIP: no reachable Docker daemon.');
    return;
  }
  buildFixtureImage();
  fs.mkdirSync(STORAGE, { recursive: true });
  const binary = awakenBin();
  const fixture = await startCalcFixture(MCP_TOKEN);
  const environment = { ...process.env };
  for (const key of [
    'AWAKEN_ACP_ARGV',
    'AWAKEN_ACP_CREDENTIAL_FILE',
    'AWAKEN_ACP_GATEWAY_URL',
    'AWAKEN_ACP_LEASE_TOKEN',
  ]) delete environment[key];
  const server = spawn(binary, {
    env: {
      ...environment,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_STORAGE_DIR: STORAGE,
      AWAKEN_ACP_CLI: 'gemini',
      AWAKEN_SANDBOX_TIER: 'docker',
      AWAKEN_CONTAINER_IMAGE: IMAGE,
      AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
      GOOGLE_GEMINI_BASE_URL: 'http://container-gateway.invalid/v1',
      GEMINI_API_KEY: 'lease-container-e2e', // awaken-allow: secret (fixture)
      GEMINI_MODEL: 'environment-fallback-must-not-win',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  let client;
  let session;

  try {
    await ready(server);
    client = new Anthropic({
      apiKey: 'e2e-dummy',
      baseURL: `http://127.0.0.1:${PORT}`,
    });
    const environmentResource = await client.beta.environments.create({
      name: `projected-container-${process.pid}`,
      config: {
        type: 'cloud',
        networking: { type: 'unrestricted' },
        // The environment declares the minimum guarantee shared by built-in tools
        // and the agent. The worker's deployment tier independently strengthens the
        // ACP process to Docker; claiming `container` here would correctly fail
        // closed because the built-in tool environment remains Workdir.
        sandbox: { isolation: 'workdir', network: { mode: 'unrestricted' } },
      },
      betas: BETAS,
    });
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('CONTAINER_INPUT_OK'), 'container-input.txt'),
      betas: BETAS,
    });
    const vault = await client.beta.vaults.create({
      display_name: 'Projected container MCP vault',
      betas: BETAS,
    });
    await client.beta.vaults.credentials.create(vault.id, {
      type: 'mcp_oauth',
      mcp_server_url: fixture.url,
      access_token: MCP_TOKEN,
      betas: BETAS,
    });
    session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: environmentResource.id,
      metadata: { 'awaken.runtime': 'acp:gemini' },
      resources: [{
        type: 'file',
        file_id: file.id,
        mount_path: '/workspace/container-input.txt',
      }],
      mcp_servers: [{
        name: 'container-fixture',
        type: 'url',
        url: fixture.url,
      }],
      vault_ids: [vault.id],
      betas: BETAS,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: 'exercise the projected container launch' }],
      }],
      betas: BETAS,
    });
    const observed = await messages(client, session.id);
    const reply = observed.texts.find((text) => text.includes('CONTAINER_PROJECTED'));
    assert.ok(
      reply,
      `the production projected container returned an ACP message: ${JSON.stringify(observed.events)}`,
    );
    assert.match(reply, /base=http:\/\/container-gateway\.invalid\/v1/u);
    assert.match(reply, /model=unconfigured/u, 'the frozen run model beats fallback env');
    assert.ok(!reply.includes('environment-fallback-must-not-win'));
    assert.match(reply, /key=lease-/u);
    assert.match(reply, /file=yes/u, 'the File binding was materialized in the container');
    assert.match(reply, /mcp=yes/u, 'the ACP session received the frozen MCP server list');
    assert.match(reply, /home=\/acp-config/u);

    console.log(
      'E2E PASS: production awaken projected model access, MCP, and File input into one Docker ACP run.',
    );
  } finally {
    // The production sandbox is Session-owned and deliberately survives server
    // shutdown for crash recovery. Dispose the Session while the server is live
    // so this E2E does not strand a container (and its writable layer) on either
    // success or an assertion failure.
    if (client && session && server.exitCode === null && server.signalCode === null) {
      await client.beta.sessions.delete(session.id, { betas: BETAS }).catch(() => {});
    }
    await stop(server).catch(() => {});
    await fixture.close();
    spawnSync('docker', ['image', 'rm', '--force', IMAGE], { stdio: 'ignore' });
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
