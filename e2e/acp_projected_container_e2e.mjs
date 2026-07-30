// Production composition E2E for a config-plane-selected ACP CLI in the Docker
// sandbox tier. Unlike managed_container_agent_e2e.mjs (the fixed newline-wire
// seam), this starts the aggregated `awaken` binary and proves that one resolved
// run projects its model access, ACP MCP delivery, and File binding into the same
// process-as-container launch.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
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
let WORKSPACE;
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const AGENT = 'projected-container-agent';

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
test "$2" = --acp
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

async function publishAgent(base, directoryUrl) {
  await request(base, 'POST', '/v1/config/provider-connections', {
    workspace_id: WORKSPACE,
    provider_id: 'gemini',
    display_name: 'Gemini',
    dialect: 'gemini',
    base_url: `${directoryUrl}/v1beta/`,
    timeout_secs: 30,
    secret: 'persisted-container-key', // awaken-allow: secret (fixture)
  });
  await request(base, 'PUT', `/v1/config/agents/${AGENT}`, {
    name: AGENT,
    model: {
      id: 'container-upstream',
      provider_identity_ref: 'gemini',
      backend_ref: 'acp:gemini',
    },
    system: 'Exercise publication-pinned container ACP provisioning.',
    tools: [],
  });
  await request(base, 'POST', `/v1/config/agents/${AGENT}/publish`);
}

async function startModelDirectory() {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ models: [{ name: 'models/container-upstream' }] }));
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  return {
    url: `http://127.0.0.1:${address.port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
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
  const anonymousFixture = await startCalcFixture('unused-container-anonymous-token', {
    allowAnonymous: true,
  });
  const directory = await startModelDirectory();
  const environment = { ...process.env };
  for (const key of [
    'AWAKEN_ACP_ARGV',
    'AWAKEN_ACP_GATEWAY_URL',
    'AWAKEN_ACP_LEASE_TOKEN',
  ]) delete environment[key];
  const configPath = path.join(TMP, 'config.toml');
  fs.writeFileSync(configPath, [
    `data_dir = ${JSON.stringify(STORAGE)}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    `control_seal_key = ${JSON.stringify(SEAL_KEY)}`,
    'sandbox_tier = "docker"',
    `container_image = ${JSON.stringify(IMAGE)}`,
    'acp_clis = ["gemini"]',
    'acp_default_cli = "gemini"',
  ].join('\n'));
  const server = spawn(binary, ['serve', '--config', configPath], {
    env: {
      ...environment,
      // Ambient values are discovery hints only. The published endpoint, model,
      // and credential revision below must be the realized runtime inputs.
      GOOGLE_GEMINI_BASE_URL: 'http://ambient-container.invalid/v1',
      GEMINI_API_KEY: 'ambient-container-must-not-win', // awaken-allow: secret (fixture)
      GEMINI_MODEL: 'environment-fallback-must-not-win',
    },
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  let client;
  let session;

  try {
    await ready(server);
    WORKSPACE = fs.readFileSync(path.join(STORAGE, 'platform-workspace-id'), 'utf8').trim();
    const base = `http://127.0.0.1:${PORT}`;
    await publishAgent(base, directory.url);
    client = new Anthropic({
      apiKey: 'e2e-dummy',
      baseURL: base,
    });
    const environmentResource = await client.beta.environments.create({
      name: `projected-container-${process.pid}`,
      config: {
        type: 'cloud',
        networking: { type: 'unrestricted' },
        // Environment owns networking. The typed deployment independently selects
        // the Docker realization tier; the public Environment DTO does not duplicate
        // private sandbox-policy fields.
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
    // Cause/effect graph / decision table for the built-in Docker provider:
    // C1=credential selected; C2=substitution; C3=no-bypass network enforcement.
    // C1 + !(C2 && C3) -> D1 reject Worker custody before container launch.
    // !C1              -> D2 inject the anonymous MCP endpoint normally.
    //
    // | Rule | credential | substitution + no-bypass | result             |
    // | D1   | yes        | no                       | fail closed         |
    // | D2   | no         | n/a                      | launch + MCP config |
    await assert.rejects(
      client.beta.sessions.create({
        agent: AGENT,
        environment_id: environmentResource.id,
        mcp_servers: [{ name: 'container-fixture-secure', type: 'url', url: fixture.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      }),
      (error) => error?.status === 500 && String(error).includes('provider-enforced secret substitution'),
      'D1: Docker must not claim Worker custody without substitution and no-bypass evidence',
    );
    session = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: environmentResource.id,
      resources: [{
        type: 'file',
        file_id: file.id,
        mount_path: '/workspace/container-input.txt',
      }],
      mcp_servers: [{
        name: 'container-fixture-anonymous',
        type: 'url',
        url: anonymousFixture.url,
      }],
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
    assert.ok(
      reply.includes(`base=${directory.url}/v1beta/`),
      `the published provider endpoint beats ambient env: ${reply}`,
    );
    assert.match(reply, /model=container-upstream/u, 'the published upstream model beats ambient env');
    assert.ok(!reply.includes('ambient-container'));
    assert.ok(!reply.includes('environment-fallback-must-not-win'));
    assert.match(reply, /key=persis/u);
    assert.match(reply, /file=yes/u, 'the File binding was materialized in the container');
    assert.match(reply, /mcp=yes/u, 'the ACP session received the frozen MCP server list');
    assert.match(reply, /home=\/workspace\/\.acp-config/u);

    console.log(
      'E2E PASS: production awaken projected publication-pinned model access, MCP, and File input into one Docker ACP run.',
    );
  } finally {
    await directory.close();
    // The production sandbox is Session-owned and deliberately survives server
    // shutdown for crash recovery. Dispose the Session while the server is live
    // so this E2E does not strand a container (and its writable layer) on either
    // success or an assertion failure.
    if (client && session && server.exitCode === null && server.signalCode === null) {
      await client.beta.sessions.delete(session.id, { betas: BETAS }).catch(() => {});
    }
    await stop(server).catch(() => {});
    await fixture.close();
    await anonymousFixture.close();
    spawnSync('docker', ['image', 'rm', '--force', IMAGE], { stdio: 'ignore' });
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
