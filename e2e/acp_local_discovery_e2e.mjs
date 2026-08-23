// Zero-configuration trusted-local ACP E2E through the shipped `awaken` binary.
//
// Cause graph:
// catalog + PATH-local CLI + CLI-owned login -> startup discovery -> exact
// wrapper acquisition -> WorkerLocal binding/Worker route -> BackendDefault
// publication -> host-HOME ACP launch. A restart reuses the resolved package;
// missing acquisition evidence fails closed into diagnostics.
//
// Decision table:
// | Rule | CLI/login | wrapper | npm/network | effect |
// | L1 | present/live | missing | fake exact installer | route + real ACP turn |
// | L2 | present/live | present | unavailable | same route/turn, no install |
// | L3 | present/live | missing | unavailable | ProbeFailed, no route |
// | L4 | absent | any | any | supported diagnostic row, not detected |
// | L5 | present, not executable | any | any | version probe failed |
// | L6 | present, times out | any | any | version probe failed |
// | L7 | present, below minimum | any | any | unsupported before login/route |
//
// Model-selection decision table:
// | Rule | policy | backend/model | effect |
// | P1 | backend default | native backend | authoring rejects; no draft |
// | P2 | backend default | bare ACP backend | authoring rejects; no draft |
// | P3 | backend default | unknown exact ACP client | publication rejects: absent executable profile |
// | P4 | exact | blank id | authoring rejects; no draft |
// | P5 | exact | unsupported exact client | publication rejects: exact selection unsupported |
// | P6 | exact | unknown WorkerLocal identity | publication rejects: no matching binding |

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import { stopServer, waitForSessionEventReceipt } from './harness.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39418);
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-local-acp-e2e-'));
const BIN_DIR = path.join(TMP, 'bin');
const HOST_HOME = path.join(TMP, 'host-home');
const DATA = path.join(TMP, 'data');
const FAILURE_DATA = path.join(TMP, 'failure-data');
const NPM_LOG = path.join(TMP, 'npm-invocations');
const CONFIG = path.join(TMP, 'config.toml');
const FAILURE_CONFIG = path.join(TMP, 'failure-config.toml');
const AGENT = 'local-codex';
const CODEX_EXACT_AGENT = 'local-codex-exact';
const GEMINI_EXACT_AGENT = 'local-gemini-exact';
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let adminToken = '';

function awakenBin() {
  return cargoExecutable({
    cwd: ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
    prebuiltEnvironmentName: AWAKEN_BIN_ENV,
  });
}

function executable(name, contents) {
  fs.mkdirSync(BIN_DIR, { recursive: true });
  const target = path.join(BIN_DIR, name);
  fs.writeFileSync(target, contents);
  fs.chmodSync(target, 0o755);
  return target;
}

function runDoctor(binary, args = ['doctor', 'acp', '--json']) {
  return spawnSync(binary, args, {
    env: localEnvironment(),
    encoding: 'utf8',
  });
}

function installFixtures() {
  fs.mkdirSync(path.join(HOST_HOME, '.codex'), { recursive: true });
  fs.writeFileSync(path.join(HOST_HOME, '.codex', 'auth.json'), 'CLI-OWNED-LOGIN');
  executable('codex', `#!/bin/sh
case "$1 $2" in
  "--version ") echo "codex 9.9.9" ;;
  "login status") echo "Logged in using ChatGPT" ;;
  *) exit 9 ;;
esac
`);
  executable('claude', `#!/bin/sh
case "$1 $2 $3" in
  "--version  ") echo "claude 9.9.9" ;;
  "auth status --json") echo '{"loggedIn":false}' ;;
  *) exit 9 ;;
esac
`);
  executable('gemini', `#!/bin/sh
case "$1" in
  --version) echo "gemini 9.9.9" ;;
  --list-sessions)
    [ -f "$HOME/gemini-login" ] && exit 0
    exit 41 ;;
  --acp)
    model=default
    [ "$2" = "--model" ] && model="$3"
    while IFS= read -r line; do
      case "$line" in
        *'"method":"initialize"'*)
          printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}' ;;
        *'"method":"session/new"'*)
          printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"local-gemini-session"}}' ;;
        *'"method":"session/prompt"'*)
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"local-gemini-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LOCAL_GEMINI model=%s"}}}}\n' "$model"
          printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}'
          exit 0 ;;
      esac
    done ;;
  *) exit 9 ;;
esac
`);
  executable('opencode', `#!/bin/sh
case "$1 $2" in
  "--version ") echo "opencode 9.9.9" ;;
  "auth list") echo "unrecognized credential output"; exit 7 ;;
  *) exit 9 ;;
esac
`);
  executable('npm', `#!/bin/sh
prefix=""
package=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --prefix) prefix="$2"; shift 2 ;;
    @*) package="$1"; shift ;;
    *) shift ;;
  esac
done
[ "$package" = "@agentclientprotocol/codex-acp@1.1.9" ] || exit 21
/bin/mkdir -p "$prefix/node_modules/.bin" || exit 22
/bin/cat > "$prefix/node_modules/.bin/codex-acp" <<'WRAPPER'
#!/bin/sh
auth=missing
[ -f "$HOME/.codex/auth.json" ] && auth=present
while IFS= read -r line; do
  id=$(/bin/sed 's/.*"id"://;s/,.*//;s/}.*//' <<EOF
$line
EOF
)
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{}}}\n' "$id" ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"local-codex-session","configOptions":[{"id":"model","name":"Model","type":"select","currentValue":"default","options":[{"value":"codex-exact","name":"Codex Exact"}]}]}}\n' "$id" ;;
    *'"method":"session/set_config_option"'*)
      model=codex-exact
      printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"local-codex-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LOCAL_ACP home=%s auth=%s model=%s"}}}}\n' "$HOME" "$auth" "\${model:-default}"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      exit 0 ;;
  esac
done
WRAPPER
/bin/chmod 755 "$prefix/node_modules/.bin/codex-acp" || exit 23
/bin/cat > "$prefix/package.json" <<'MANIFEST'
{"dependencies":{"@agentclientprotocol/codex-acp":"1.1.9"}}
MANIFEST
printf '%s\n' "$package" >> ${JSON.stringify(NPM_LOG)}
`);
}

function writeConfig(target, dataDir, cliIds = ['codex', 'gemini']) {
  fs.writeFileSync(target, [
    `data_dir = ${JSON.stringify(dataDir)}`,
    `bind = ${JSON.stringify(`127.0.0.1:${PORT}`)}`,
    'control_seal_key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"',
    'sandbox_tier = "local"',
    `acp_clis = ${JSON.stringify(cliIds)}`,
  ].join('\n'));
}

function localEnvironment() {
  return {
    ...process.env,
    // Every fixture uses an absolute /bin/sh shebang and absolute helper paths.
    // Keeping PATH closed proves no system npm/network fallback is possible.
    PATH: BIN_DIR,
    HOME: HOST_HOME,
  };
}

function start(binary, config) {
  const child = spawn(binary, automatedAllInOneArgs('--config', config), {
    env: localEnvironment(),
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  return child;
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
    await sleep(100);
  }
  throw new Error('awaken did not become ready');
}

async function request(method, route, body) {
  const response = await fetch(`http://127.0.0.1:${PORT}${route}`, {
    method,
    headers: {
      authorization: `Bearer ${adminToken}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  return { response, value };
}

async function capability(cli) {
  const { response, value } = await request('GET', '/v1/capabilities');
  assert.equal(response.status, 200, JSON.stringify(value));
  return value.runtimes.find((runtime) => runtime.id === `acp:${cli}`);
}

async function waitForCapability(cli, predicate, timeoutMs = 15_000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    last = await capability(cli);
    if (last && predicate(last)) return last;
    await sleep(100);
  }
  return last;
}

async function assertPublicationRejected(id, model, reason) {
  const authored = await request('PUT', `/v1/config/agents/${id}`, {
    name: id,
    model,
    tools: [],
  });
  assert.equal(authored.response.status, 200, JSON.stringify(authored.value));
  const published = await request('POST', `/v1/config/agents/${id}/publish`);
  assert.equal(published.response.status, 409, JSON.stringify(published.value));
  assert.match(JSON.stringify(published.value), reason);
}

async function assertAuthoringRejected(id, model, reason) {
  const authored = await request('PUT', `/v1/config/agents/${id}`, {
    name: id,
    model,
    tools: [],
  });
  assert.equal(authored.response.status, 400, JSON.stringify(authored.value));
  assert.match(JSON.stringify(authored.value), reason);
  const projected = await request('GET', `/v1/config/agents/${id}`);
  assert.equal(projected.response.status, 404, JSON.stringify(projected.value));
}

async function runLocalTurn(
  agent = AGENT,
  expected = `LOCAL_ACP home=${HOST_HOME} auth=present`,
) {
  const client = new Anthropic({
    apiKey: adminToken,
    baseURL: `http://127.0.0.1:${PORT}`,
  });
  const session = await client.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  // C1=exact local-ACP User receipt; C2=CLI-owned login reply+terminal. E1=C2
  // after C1 proves this discovered wrapper. K: discovery/acquisition evidence
  // stays outside Session history. Decision L1 C1&&!C2=>retry; L2 C1+C2=>proof.
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'prove local login' }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'L1 exact local ACP User Event receipt');
  const { delta } = await waitForSessionEventReceipt(
    client,
    session.id,
    receiptId,
    BETAS,
    ({ delta: later }) => later.some((event) => event.type === 'agent.message')
      && later.some((event) => event.type === 'session.status_idle'),
    'L1 trusted local ACP Run to commit its login proof',
  );
  const texts = [];
  for (const event of delta) {
    if (event.type === 'agent.message') {
      texts.push(...(event.content ?? []).map((content) => content.text ?? ''));
    }
  }
  assert.ok(
    texts.some((text) => text.includes(expected)),
    `trusted local ACP did not produce ${expected}: ${JSON.stringify(texts)}`,
  );
}

async function main() {
  const binary = awakenBin();
  installFixtures();
  writeConfig(CONFIG, DATA);
  writeConfig(FAILURE_CONFIG, FAILURE_DATA, ['codex']);

  // L4 plus discovery/acquisition separation: doctor sees the CLI login without
  // consulting npm. Temporarily hide the installer before any wrapper exists.
  fs.renameSync(path.join(BIN_DIR, 'npm'), path.join(BIN_DIR, 'npm.hidden'));
  const opencodePath = path.join(BIN_DIR, 'opencode');
  const opencodeContents = fs.readFileSync(opencodePath, 'utf8');
  fs.renameSync(opencodePath, `${opencodePath}.hidden`);
  const missingDoctor = runDoctor(binary);
  assert.equal(missingDoctor.status, 0, missingDoctor.stderr);
  const missingDiagnostic = JSON.parse(missingDoctor.stdout);
  assert.equal(missingDiagnostic.acp.find((row) => row.id === 'opencode').detected, false, 'L4');
  assert.equal(
    missingDiagnostic.acp.find((row) => row.id === 'opencode').reason_code,
    'acp_agent_missing',
    'L4',
  );

  fs.renameSync(`${opencodePath}.hidden`, opencodePath);
  fs.chmodSync(opencodePath, 0o644);
  const unexecutableDoctor = runDoctor(binary);
  assert.equal(unexecutableDoctor.status, 0, unexecutableDoctor.stderr);
  assert.equal(
    JSON.parse(unexecutableDoctor.stdout).acp.find((row) => row.id === 'opencode').reason_code,
    'acp_version_probe_failed',
    'L5',
  );

  executable('opencode', '#!/bin/sh\n/bin/sleep 10\n');
  const timeoutDoctor = runDoctor(binary);
  assert.equal(timeoutDoctor.status, 0, timeoutDoctor.stderr);
  assert.equal(
    JSON.parse(timeoutDoctor.stdout).acp.find((row) => row.id === 'opencode').reason_code,
    'acp_version_probe_failed',
    'L6',
  );
  executable('opencode', `#!/bin/sh
case "$1 $2" in
  "--version ") echo "opencode 1.18.11" ;;
  "auth list") echo "1 credential" ;;
  *) exit 9 ;;
esac
`);
  const outdatedDoctor = runDoctor(binary);
  assert.equal(outdatedDoctor.status, 0, outdatedDoctor.stderr);
  const outdated = JSON.parse(outdatedDoctor.stdout).acp.find((row) => row.id === 'opencode');
  assert.equal(outdated.detected, false, 'L7');
  assert.equal(outdated.login_state, null, 'L7: login is intentionally not probed');
  assert.equal(outdated.reason_code, 'acp_version_unsupported', 'L7');
  assert.equal(outdated.version, 'opencode 1.18.11', 'L7');
  executable('opencode', opencodeContents);

  const doctor = runDoctor(binary);
  assert.equal(doctor.status, 0, doctor.stderr);
  const diagnostic = JSON.parse(doctor.stdout);
  assert.equal(diagnostic.acp.find((row) => row.id === 'codex').login_state, 'available');
  assert.equal(diagnostic.acp.find((row) => row.id === 'claude').login_state, 'login_required');
  assert.equal(diagnostic.acp.find((row) => row.id === 'gemini').login_state, 'login_required');
  assert.equal(diagnostic.acp.find((row) => row.id === 'opencode').login_state, 'probe_failed');

  // The human diagnostic is a first-class public surface, not a JSON formatting
  // fallback. It must carry status and catalog-owned remediation for detected and
  // missing clients without exposing any credential content.
  const humanDoctor = spawnSync(binary, ['doctor', 'acp'], {
    env: localEnvironment(),
    encoding: 'utf8',
  });
  assert.equal(humanDoctor.status, 0, humanDoctor.stderr);
  assert.match(humanDoctor.stdout, /codex\s+available\s+codex 9\.9\.9/);
  assert.match(humanDoctor.stdout, /claude\s+login_required\s+claude 9\.9\.9/);
  assert.match(humanDoctor.stdout, /Run `claude auth login`/);
  assert.match(humanDoctor.stdout, /gemini\s+login_required\s+gemini 9\.9\.9/);
  assert.match(humanDoctor.stdout, /opencode\s+probe_failed\s+opencode 9\.9\.9/);
  assert.doesNotMatch(humanDoctor.stdout, /CLI-OWNED-LOGIN/);

  for (const [args, expected] of [
    [['doctor'], "doctor requires the 'acp' subject"],
    [['doctor', 'models'], 'unknown doctor subject'],
    [['doctor', 'acp', '--bogus'], 'unexpected doctor acp argument'],
  ]) {
    const rejected = spawnSync(binary, args, { env: localEnvironment(), encoding: 'utf8' });
    assert.notEqual(rejected.status, 0, `${args.join(' ')} unexpectedly succeeded`);
    assert.match(`${rejected.stdout}\n${rejected.stderr}`, new RegExp(expected));
  }
  const help = spawnSync(binary, ['doctor', 'acp', '--help'], {
    env: localEnvironment(),
    encoding: 'utf8',
  });
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /doctor acp/);
  fs.renameSync(path.join(BIN_DIR, 'npm.hidden'), path.join(BIN_DIR, 'npm'));
  // Simulate the user completing Gemini's own login before server startup. The
  // same provider-owned probe must then publish it as an executable WorkerLocal
  // capability; Awaken still never opens a Gemini credential file.
  fs.writeFileSync(path.join(HOST_HOME, 'gemini-login'), 'provider-owned-login');

  let server = start(binary, CONFIG);
  try {
    await ready(server);
    adminToken = fs.readFileSync(path.join(DATA, 'admin-token'), 'utf8').trim();
    const codex = await waitForCapability('codex', (row) => row.local.detected);
    assert.equal(codex.local.detected, true, 'L1');
    assert.equal(codex.local.login_state, 'available', 'L1');
    assert.deepEqual(fs.readFileSync(NPM_LOG, 'utf8').trim().split('\n'), [
      '@agentclientprotocol/codex-acp@1.1.9',
    ]);

    let result = await request('PUT', `/v1/config/agents/${AGENT}`, {
      name: 'Local Codex',
      system: 'Use the already logged-in local coding agent.',
      model: { mode: 'backend_default', backend_ref: 'acp:codex' },
      tools: [],
    });
    assert.equal(result.response.status, 200, JSON.stringify(result.value));
    result = await request('POST', `/v1/config/agents/${AGENT}/publish`);
    assert.equal(result.response.status, 200, JSON.stringify(result.value));
    // The authoring route preserves the backend-default policy. The Managed
    // Agent wire has only ModelConfig{id,...}, so it projects the resolved
    // snapshot separately and is not a second authoring representation.
    const projected = await request('GET', `/v1/config/agents/${AGENT}`);
    assert.equal(projected.response.status, 200, JSON.stringify(projected.value));
    assert.deepEqual(projected.value.model, {
      mode: 'backend_default',
      backend_ref: 'acp:codex',
    });

    const ambiguous = await request('PUT', '/v1/config/agents/ambiguous-local-model', {
      name: 'Ambiguous local model',
      model: {
        mode: 'backend_default',
        backend_ref: 'acp:codex',
        id: 'must-not-coexist',
      },
      tools: [],
    });
    assert.equal(ambiguous.response.status, 400, JSON.stringify(ambiguous.value));

    await assertAuthoringRejected(
      'local-native-default',
      { mode: 'backend_default', backend_ref: 'genai' },
      /invalid backend reference.*genai/,
    );
    await assertAuthoringRejected(
      'local-bare-acp-default',
      { mode: 'backend_default', backend_ref: 'acp' },
      /invalid backend reference.*acp/,
    );
    await assertPublicationRejected(
      'local-unknown-acp-default',
      { mode: 'backend_default', backend_ref: 'acp:unknown' },
      /is not in the executable catalog/,
    );
    await assertAuthoringRejected(
      'local-blank-exact',
      { mode: 'backend_exact', model_ref: '', backend_ref: 'acp:codex' },
      /backend-exact model requires a non-empty model_ref/,
    );
    await assertPublicationRejected(
      'local-unsupported-exact',
      { mode: 'backend_exact', model_ref: 'opencode-exact', backend_ref: 'acp:opencode' },
      /cannot guarantee an exact model selection/,
    );
    await assertPublicationRejected(
      'local-missing-identity',
      {
        id: 'codex-exact',
        provider_identity_ref: 'worker-local:does-not-exist',
        backend_ref: 'acp:codex',
      },
      /no active Worker-local binding is registered/,
    );

    const malformedModel = await request('PUT', '/v1/config/agents/malformed-model', {
      name: 'Malformed model',
      model: 42,
      tools: [],
    });
    assert.equal(malformedModel.response.status, 400, JSON.stringify(malformedModel.value));
    await runLocalTurn();

    for (const [agent, name, model, expected] of [
      [
        CODEX_EXACT_AGENT,
        'Local Codex exact',
        { mode: 'backend_exact', model_ref: 'codex-exact', backend_ref: 'acp:codex' },
        'model=codex-exact',
      ],
      [
        GEMINI_EXACT_AGENT,
        'Local Gemini exact',
        { mode: 'backend_exact', model_ref: 'gemini-exact', backend_ref: 'acp:gemini' },
        'LOCAL_GEMINI model=gemini-exact',
      ],
    ]) {
      result = await request('PUT', `/v1/config/agents/${agent}`, {
        name,
        system: 'Use the exact backend-owned model without provider material.',
        model,
        tools: [],
      });
      assert.equal(result.response.status, 200, JSON.stringify(result.value));
      result = await request('POST', `/v1/config/agents/${agent}/publish`);
      assert.equal(result.response.status, 200, JSON.stringify(result.value));
      await runLocalTurn(agent, expected);
    }
  } finally {
    await stopServer(server);
  }

  // L2 causes: C1 the first healthy all-in-one Worker drains and deregisters;
  // C2 npm is absent; C3 the wrapper, publication, and binding are durable.
  // Effect: the replacement Worker registers without a stale-lease delay and
  // executes the same real ACP turn from the installed absolute wrapper.
  // Constraint: crash/lease-expiry recovery is a separate Worker lifecycle
  // rule, so this ordinary restart must use the canonical graceful stop path.
  // Decision L2: C1+C2+C3 => one offline restart turn and no second install.
  fs.renameSync(path.join(BIN_DIR, 'npm'), path.join(BIN_DIR, 'npm.hidden'));
  server = start(binary, CONFIG);
  try {
    await ready(server);
    adminToken = fs.readFileSync(path.join(DATA, 'admin-token'), 'utf8').trim();
    assert.equal(
      (await waitForCapability('codex', (row) => row.local.detected)).local.detected,
      true,
      'L2',
    );
    await runLocalTurn();
    assert.equal(fs.readFileSync(NPM_LOG, 'utf8').trim().split('\n').length, 1, 'L2');
  } finally {
    await stopServer(server);
  }

  // L3: a fresh data directory has no wrapper and no installer. Product startup
  // remains available for diagnostics but cannot advertise an ACP execution route.
  server = start(binary, FAILURE_CONFIG);
  try {
    await ready(server);
    adminToken = fs.readFileSync(path.join(FAILURE_DATA, 'admin-token'), 'utf8').trim();
    const codex = await waitForCapability(
      'codex',
      (row) => row.local.reason_code === 'acp_wrapper_install_failed',
    );
    assert.equal(codex.local.detected, false, 'L3');
    assert.equal(codex.local.login_state, 'probe_failed', 'L3');
    assert.equal(codex.local.reason_code, 'acp_wrapper_install_failed', 'L3');
  } finally {
    await stopServer(server);
  }

  console.log('LOCAL ACP DISCOVERY E2E PASS: exact install, CLI-owned login, offline restart, and fail-closed diagnostics.');
}

main()
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  })
  .finally(() => fs.rmSync(TMP, { recursive: true, force: true }));
