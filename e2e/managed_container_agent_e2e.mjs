// Full external SDK -> managed -> CONTAINER agent, against a real Docker or Podman daemon.
//
// The deepest sandbox seam: an ACP agent running as a **process-as-container** in a
// real container, driven end-to-end through the managed protocol. The brain
// (scenario-host, `AWAKEN_MODEL_MODE=acp-container`, built with the selected backend)
// creates one Session-owned environment, starts the production hand in it, and execs
// a deterministic newline ACP fixture in that SAME environment. Seeing the fixture's
// marker proves: external SDK -> managed session -> environment create -> bound ACP
// exec -> response; inspecting the container proves no per-attempt environment exists.
//
// The k8s POD mechanics of the same seam are covered by the k8s adapter e2e
// (`awaken-sandbox-container/tests/k8s_e2e.rs`) + the k3d topology e2e; Docker and
// Podman keep this managed-protocol proof to a single daemon (no cluster).
//
// Select Podman with `AWAKEN_E2E_CONTAINER_ENGINE=podman`. The standalone npm suite
// self-skips when the selected engine is unavailable; the stage gate sets
// `AWAKEN_E2E_REQUIRE_CONTAINER=1` and fails closed instead.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn, execFileSync as rawExecFileSync, spawnSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { ensureCanonicalSandboxImage } from './fixtures/sandbox_image.mjs';
import {
  committedEffectsAfterUnanchoredReceipt,
  REPO_ROOT,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';
import { cargoExecutable } from './cargo_binary.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38143);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const MARKER = 'CONTAINER-AGENT-OK';
const ENGINE = process.env.AWAKEN_E2E_CONTAINER_ENGINE ?? 'docker';
assert.ok(['docker', 'podman'].includes(ENGINE), `unsupported container engine ${ENGINE}`);
const IMAGE = process.env.AWAKEN_TEST_SESSION_IMAGE ?? 'awaken-sandbox:session-e2e';
const PACKAGE_BASE_IMAGE = `${IMAGE}-package-base`;
const ENGINE_TIMEOUT_MS = 30_000;
const TMP = path.join(os.tmpdir(), `awaken-container-agent-${ENGINE}-e2e-${process.pid}`);
const ACP_FIXTURE = `process.stdin.once('data',()=>{console.log(JSON.stringify({type:'message',text:'${MARKER}'}));console.log(JSON.stringify({type:'turn_end',reason:'natural_end'}))})`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function listSessionEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

const PACKAGE_CASES = [
  { manager: 'apt', requirements: ['jq'], proof: 'jq --version' },
  { manager: 'cargo', requirements: ['minigrep@0.1.0'], proof: 'test -x /usr/local/bin/minigrep' },
  { manager: 'gem', requirements: ['rake:13.4.2'], proof: 'rake --version' },
  { manager: 'go', requirements: ['github.com/rakyll/hey@v0.1.4'], proof: 'test -x /usr/local/bin/hey' },
  { manager: 'npm', requirements: ['cowsay@1.6.0'], proof: 'cowsay AWAKEN | grep AWAKEN' },
  {
    manager: 'pip',
    requirements: ['cowsay==6.1'],
    proof: '/opt/awaken-python/bin/python -c \'import cowsay; print(cowsay.get_output_string("cow", "AWAKEN"))\' | grep AWAKEN',
  },
];

function execFileSync(file, args, options = {}) {
  return rawExecFileSync(file, args, file === ENGINE
    ? { ...options, timeout: ENGINE_TIMEOUT_MS }
    : options);
}

async function afterPendingActivation(operation) {
  let last;
  for (let attempt = 0; attempt < 50; attempt += 1) {
    try {
      return await operation();
    } catch (error) {
      last = error;
      if (error.status !== 400 || !String(error.message).includes('activation is already pending')) throw error;
      await sleep(100);
    }
  }
  throw last;
}

async function exerciseContainerEnvironment(
  client, name, sandbox, expectSuccess,
  {
    packages,
    expectedMarker = MARKER,
    proveImageReuse = false,
    proofCommands = [],
  } = {},
) {
  const network = sandbox.network;
  const environment = await client.beta.environments.create({
    name: `podman-${name}`,
    config: {
      type: 'cloud',
      networking: !network || network.mode === 'unrestricted'
        ? { type: 'unrestricted' }
        : { type: 'limited', allowed_hosts: network.hosts ?? [] },
      ...(packages ? { packages: { type: 'packages', ...packages } } : {}),
    },
    betas: BETAS,
  });
  const policyId = `podman-${name}-${process.pid}`;
  const { network: _ownedByEnvironment, ...policyConfig } = sandbox;
  let response = await fetch(`http://127.0.0.1:${PORT}/v1/awaken/sandbox-execution-policies`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ id: policyId, config: policyConfig }),
  });
  assert.equal(response.status, 201, await response.text());
  response = await fetch(
    `http://127.0.0.1:${PORT}/v1/awaken/environments/${environment.id}/sandbox-execution-policy`,
    {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ policy_id: policyId, version: 1 }),
    },
  );
  assert.equal(response.status, 200, await response.text());
  const realizedImages = [];
  const realizedContainers = [];
  const attempts = proveImageReuse ? 2 : 1;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const containersBefore = new Set(testContainerNames());
    const session = await client.beta.sessions.create({
      agent: 'namespace-agent',
      environment_id: environment.id,
      betas: BETAS,
    });
    let sendFailure;
    let receipt;
    try {
      receipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: `exercise ${name}` }] }],
        betas: BETAS,
      });
    } catch (error) {
      sendFailure = error;
    }
    let events = [];
    if (expectSuccess) {
      assert.equal(sendFailure, undefined, `${name} should realize: ${sendFailure}`);
      const acceptedId = receipt?.data?.[0]?.id;
      assert.equal(typeof acceptedId, 'string', `${name} returns one exact User Event receipt`);
      ({ events } = await waitForSessionEventReceipt(
        client,
        session.id,
        acceptedId,
        BETAS,
        ({ delta }) => delta.some(
          (event) => event.type === 'agent.message'
            && (event.content ?? []).some((content) => String(content.text ?? '').includes(expectedMarker)),
        ),
        `${name} container Run to commit its Agent marker`,
        { timeoutMs: 180_000, pollMs: 100 },
      ));
      assert.ok(
        events.some(
          (event) => event.type === 'agent.message'
            && (event.content ?? []).some((content) => String(content.text ?? '').includes(expectedMarker)),
        ),
        `${name} must run the containerized agent: ${JSON.stringify(events)}`,
      );
      if (proveImageReuse || proofCommands.length > 0) {
        // Container ownership cause/effect rule: C1 any unrelated pre-existing
        // test containers + C2 one successful Session realization => E1 exactly
        // one new container. Use the observable set delta; the runtime's opaque,
        // collision-safe name is not an API contract and must not be reimplemented
        // here as a suffix assumption.
        const names = testContainerNames()
          .filter((candidate) => !containersBefore.has(candidate));
        assert.equal(names.length, 1, `${name} session must own one fresh container: ${names}`);
        for (const command of proofCommands) {
          execFileSync(ENGINE, ['exec', names[0], 'sh', '-lc', command], { encoding: 'utf8' });
        }
        if (proveImageReuse) {
          realizedContainers.push(names[0]);
          realizedImages.push(
            execFileSync(ENGINE, ['inspect', '--format', '{{.Config.Image}}|{{.Image}}|{{.Config.User}}', names[0]], {
              encoding: 'utf8',
            }).trim(),
          );
          assert.match(
            realizedImages.at(-1),
            /\|10001$/,
            `${name} package build must restore the base image's non-root runtime user`,
          );
          if (process.env.AWAKEN_PACKAGE_IMAGE_REGISTRY) {
            assert.match(
              realizedImages.at(-1).split('|')[0],
              new RegExp(`^${process.env.AWAKEN_PACKAGE_IMAGE_REGISTRY
                .replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}/awaken-packages@sha256:`),
              `${name} must run the registry-published immutable digest`,
            );
          }
          if (process.env.AWAKEN_PACKAGE_REGISTRY_AUTH_FILE) {
            const inspect = execFileSync(
              ENGINE,
              ['inspect', '--format', '{{json .Config.Env}}|{{json .Mounts}}', names[0]],
              { encoding: 'utf8' },
            );
            assert.ok(
              !inspect.includes(process.env.AWAKEN_PACKAGE_REGISTRY_AUTH_FILE)
                && !inspect.includes('test-secret')
                && !inspect.includes('YXdha2VuOnRlc3Qtc2VjcmV0'),
              `${name} registry credentials must remain in the Worker-side builder`,
            );
          }
        }
      }
    } else {
      if (!sendFailure) {
        const acceptedId = receipt?.data?.[0]?.id;
        assert.equal(typeof acceptedId, 'string', `${name} returns one exact User Event receipt`);
        assert.equal(
          receipt.data[0].processed_at,
          null,
          `${name} capability effect is not falsely processed`,
        );
        // Failure decision rules: F1 synchronous admission error => surface it;
        // F2 accepted command + retryable provider/capability failure => return
        // the exact unprocessed admission receipt, exclude it from unanchored
        // committed history, and create no Agent/terminal effect over one bounded
        // reconciliation window. F2 is not a session.error until a separate
        // authority classifies the fault as permanently quarantined.
        await new Promise((resolve) => setTimeout(resolve, 750));
        events = await listSessionEvents(client, session.id);
        committedEffectsAfterUnanchoredReceipt({
          history: events,
          receiptId: acceptedId,
          forbiddenEventTypes: new Set([
            'agent.message',
            'agent.tool_use',
            'agent.tool_result',
            'session.error',
            'session.status_idle',
            'session.usage',
            'span.model_request_start',
            'span.model_request_end',
          ]),
          description: `${name} retryable provider failure`,
        });
      }
    }
    // Cleanup decision rules: C1 successful effects settled => ordinary Session
    // delete/release is valid; C2 an expected retryable capability failure left
    // the exact receipt unsettled => a terminal delete must remain unavailable.
    // The process-scoped fixture directory owns C2 teardown after the assertion;
    // attempting an API delete here would contradict the fail-closed invariant
    // this branch just proved.
    if (expectSuccess) {
      await client.beta.sessions.delete(session.id, { betas: BETAS });
    }
  }
  if (proveImageReuse) {
    assert.equal(realizedImages.length, 2);
    assert.notEqual(
      realizedContainers[1],
      realizedContainers[0],
      `${name} sessions must remain isolated in distinct containers`,
    );
    assert.equal(
      realizedImages[1],
      realizedImages[0],
      `${name} must reuse the exact content-addressed image while keeping sessions isolated`,
    );
  }
  if (expectSuccess) {
    await client.beta.environments.delete(environment.id, { betas: BETAS });
  }
}

async function exercisePackageManagerMatrix(client, { registryOnly = false } = {}) {
  const cases = registryOnly
    ? PACKAGE_CASES.filter(({ manager }) => manager === 'npm')
    : PACKAGE_CASES;
  for (const testCase of cases) {
    await exerciseContainerEnvironment(client, `${ENGINE}-package-${testCase.manager}`, {
      environment: { kind: 'image', reference: PACKAGE_BASE_IMAGE },
    }, true, {
      packages: { [testCase.manager]: testCase.requirements },
      proveImageReuse: registryOnly,
      proofCommands: ['test -x /usr/local/bin/awaken-sandbox', testCase.proof],
    });
    console.log(`  ok: ${testCase.manager} installed ${testCase.requirements.join(', ')}`);
  }
}

async function exercisePodmanRootfsMatrix(client) {
  // Network admission cause/effect graph:
  // C1=allowlist requested; C2=provider proves no-bypass enforcement.
  // C1+!C2 -> N1 fail closed with retained retryable custody and no fallback.
  // !C1 -> N2 proceed using the selected rootfs.
  //
  // | Rule | network request | provider proof | result              |
  // | N1   | allowlist       | absent         | reject, no fallback |
  // | N2   | none/default    | n/a            | realize rootfs      |
  await exerciseContainerEnvironment(client, 'host-userland', {
    environment: { kind: 'sandbox' },
    network: { mode: 'allowlist', hosts: ['example.invalid'] },
    limits: { cpu_millis: 1000, memory_bytes: 536870912, pids: 128 },
  }, false);
  await exerciseContainerEnvironment(client, 'explicit-image', {
    environment: { kind: 'image', reference: IMAGE },
    network: { mode: 'none' },
  }, true);
  // Package provisioning cause graph:
  // exact Environment packages + exact image root -> content-addressed derived
  // image -> package build completes -> workload observes the installed effect.
  // No package adapter/capability means fail closed before workload creation
  // (covered by the Rust provider decision table).
  //
  // | Rule | provider capability | requirements | observable behavior |
  // | P1   | Podman/Docker: present | exact npm pin | workload observes installed binary |
  // | P3   | either               | absent        | ordinary selected-image execution |
  await exerciseContainerEnvironment(client, 'package-image', {
    environment: { kind: 'image', reference: PACKAGE_BASE_IMAGE },
  }, true, {
    packages: { npm: ['cowsay@1.6.0'] },
    proveImageReuse: true,
    proofCommands: [
      'test -x /usr/local/bin/awaken-sandbox',
      'cowsay AWAKEN | grep AWAKEN',
    ],
  });
  await exerciseContainerEnvironment(client, 'scope-fallback', {
    environment: { kind: 'scope' },
  }, true);
  await exerciseContainerEnvironment(client, 'local-dir-fallback', {
    environment: { kind: 'local_dir', path_template: '/tmp/not-a-container-root' },
  }, true);
  await exerciseContainerEnvironment(client, 'readonly-root', {
    environment: {
      kind: 'isolated_root',
      base: { source: 'dir', path_template: `${TMP}/missing-readonly-root` },
      writable_base: false,
    },
  }, false);
  await exerciseContainerEnvironment(client, 'writable-root', {
    environment: {
      kind: 'isolated_root',
      base: { source: 'dir', path_template: `${TMP}/missing-writable-root` },
      writable_base: true,
    },
  }, false);
  await exerciseContainerEnvironment(client, 'tarball-root', {
    environment: {
      kind: 'isolated_root',
      base: { source: 'tarball', reference: `${TMP}/missing-root.tar` },
      writable_base: false,
    },
  }, false);
  const remaining = testContainerNames({ all: true });
  assert.ok(
    remaining.every((name) => name.includes('-warmpool_')),
    `every rootfs matrix Session must be released; only unused warm capacity may remain: ${remaining}`,
  );
}

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8' });
}

function seedSkillRepository() {
  const work = `${TMP}/skill-seed`;
  fs.mkdirSync(`${work}/greet`, { recursive: true });
  git(['init', '-q'], work);
  git(['symbolic-ref', 'HEAD', 'refs/heads/main'], work);
  git(['config', 'user.email', 'container-e2e@awaken.invalid'], work);
  git(['config', 'user.name', 'Awaken Container E2E'], work);
  fs.writeFileSync(
    `${work}/greet/SKILL.md`,
    '---\nname: greet\ndescription: container skill\n---\nCONTAINER-SKILL-OK\n',
  );
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed container skill'], work);
  const bare = `${TMP}/skills.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

function containerAvailable() {
  // A CLI can be installed while its daemon is unreachable. Bound the probe so
  // this optional E2E reaches its documented skip/fail-closed branch instead of
  // hanging the complete causal suite indefinitely.
  return spawnSync(ENGINE, ['version'], {
    stdio: 'ignore',
    timeout: ENGINE_TIMEOUT_MS,
  }).status === 0;
}

function testContainers({ all = false } = {}) {
  const args = ['ps'];
  if (all) args.push('-a');
  args.push('--format', '{{.ID}}|{{.Image}}|{{.Names}}', '--filter', 'label=awaken.sandbox=1');
  return execFileSync(ENGINE, args, { encoding: 'utf8', timeout: ENGINE_TIMEOUT_MS })
    .trim().split('\n').filter(Boolean)
    .filter((row) => testContainerImage(row.split('|')[1]))
    .map((row) => row.split('|')[0]);
}

function testContainerNames({ all = false } = {}) {
  const args = ['ps'];
  if (all) args.push('-a');
  args.push('--format', '{{.ID}}|{{.Image}}|{{.Names}}', '--filter', 'label=awaken.sandbox=1');
  return execFileSync(ENGINE, args, { encoding: 'utf8', timeout: ENGINE_TIMEOUT_MS })
    .trim().split('\n').filter(Boolean)
    .filter((row) => testContainerImage(row.split('|')[1]))
    .map((row) => row.split('|')[2]);
}

function testContainerImage(image) {
  // Engine display decision table: Docker preserves an unqualified local tag;
  // Podman renders that same local store tag with its implicit `localhost/`
  // registry. Normalize only that Podman-owned prefix; explicit registries and
  // immutable digest references remain exact evidence.
  const displayed = ENGINE === 'podman' && image.startsWith('localhost/')
    ? image.slice('localhost/'.length)
    : image;
  return displayed === IMAGE
    || displayed === PACKAGE_BASE_IMAGE
    || displayed.startsWith('awaken-packages:')
    // Docker's `ps --format {{.Image}}` drops the digest for containers created
    // from a registry digest even though inspect retains the immutable reference.
    || displayed.endsWith('/awaken-packages')
    || displayed.includes('/awaken-packages@sha256:');
}

function cleanupTestContainers() {
  const containers = testContainers({ all: true });
  if (containers.length > 0) {
    spawnSync(ENGINE, ['rm', '-f', ...containers], { stdio: 'ignore', timeout: ENGINE_TIMEOUT_MS });
  }
}

async function waitForTestContainersToBeReaped(timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  let containers = testContainers({ all: true });
  while (containers.length > 0 && Date.now() < deadline) {
    await sleep(100);
    containers = testContainers({ all: true });
  }
  assert.deepEqual(containers, [], 'Session release must reap its container within the bound');
}

// Build the canonical production image, but omit network-fetched ACP packages: this
// hermetic dev scenario supplies a tiny Node newline fixture through its explicit
// fixed launch input (read from AWAKEN_ACP_ARGV only by the scenario host).
// Image-source decision rules: Q1 Docker + fully qualified public bases => build;
// Q2 Podman with no unqualified registry + the same bases => build; Q3 either
// engine cannot resolve an exact base => fail before any Session effect. The
// image still contains the real `awaken-sandbox hand --stdio` binary.
function ensureSessionImage() {
  ensureCanonicalSandboxImage({
    engine: ENGINE,
    image: IMAGE,
    repoRoot: REPO_ROOT,
    timeoutMs: ENGINE_TIMEOUT_MS,
  });
}

function ensurePackageFixtureImage() {
  const existing = spawnSync(
    ENGINE,
    [
      'image', 'inspect', '--format',
      '{{.Config.User}}|{{index .Config.Labels "org.awaken.playwright-mcp-fixture"}}',
      PACKAGE_BASE_IMAGE,
    ],
    { encoding: 'utf8', timeout: ENGINE_TIMEOUT_MS },
  );
  if (existing.status === 0 && existing.stdout.trim() === '10001|1') return;
  const containerfile = [
    `FROM ${IMAGE}`,
    'USER root',
    'COPY playwright_acp_mcp_fixture.mjs /usr/local/bin/awaken-playwright-acp-fixture',
    'RUN ["chmod","0755","/usr/local/bin/awaken-playwright-acp-fixture"]',
    'LABEL org.awaken.playwright-mcp-fixture="1"',
    'USER 10001',
    '',
  ].join('\n');
  const context = `${TMP}/package-base`;
  fs.mkdirSync(context, { recursive: true });
  fs.copyFileSync(
    path.join(REPO_ROOT, 'e2e/fixtures/playwright_acp_mcp_fixture.mjs'),
    path.join(context, 'playwright_acp_mcp_fixture.mjs'),
  );
  const containerfilePath = `${context}/Containerfile`;
  fs.writeFileSync(containerfilePath, containerfile);
  // A selected docker-container buildx builder cannot resolve daemon-local base
  // images and does not automatically load its result. Use Docker's local driver
  // explicitly; Podman already builds against and writes to its local image store.
  const buildArgs = ENGINE === 'docker'
    ? ['buildx', 'build', '--builder', 'default', '--load']
    : ['build'];
  const result = spawnSync(ENGINE, [...buildArgs, '--tag', PACKAGE_BASE_IMAGE, '--file', containerfilePath, context], {
    cwd: REPO_ROOT,
    encoding: 'utf8',
    timeout: ENGINE_TIMEOUT_MS,
  });
  assert.equal(result.status, 0, `package fixture image build failed: ${result.stderr}`);
}

// Build the brain with the selected container feature (the shared harness builds
// default features only), and resolve the binary path from cargo's JSON output.
function buildBrain() {
  return cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-scenario-host',
    targetName: 'awaken-scenario-host',
    features: [`container-${ENGINE}`],
  });
}

async function main() {
  // Test design (container matrix). Causes: C1=the selected Docker/Podman engine
  // is reachable; C2=package/image/rootfs and immutable Environment variants;
  // C3=ACP, Native, local MCP, Hand, File, Memory, Repository, Skill, and browser
  // resources are frozen on the Session. Effects: E1=!C1 skips or fails when
  // explicitly required; E2=each C2 arm realizes and cleans its exact container;
  // E3=C3 is observable only inside that Session's shared sandbox.
  // Constraints/invariant: Environment/resource manifests and one Session
  // container are authoritative; no host fallback or cross-Session leak is valid.
  // Decision rules: K0=!C1=>skip|required-fail; K1=C1+C2=>E2;
  // K2=C1+C2+C3=>E2+E3.
  if (!containerAvailable()) {
    if (process.env.AWAKEN_E2E_REQUIRE_CONTAINER === '1') {
      throw new Error(`required ${ENGINE} runtime is unavailable`);
    }
    console.log(`E2E SKIP: no reachable ${ENGINE} runtime.`);
    return;
  }
  cleanupTestContainers();
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  ensureSessionImage();
  ensurePackageFixtureImage();
  const skillRepository = seedSkillRepository();
  const bin = buildBrain();
  const addr = `127.0.0.1:${PORT}`;
  const brainEnv = {
    ...process.env,
    AWAKEN_HTTP_ADDR: addr,
    AWAKEN_MODEL_MODE: 'acp-container',
    AWAKEN_CONTAINER_IMAGE: IMAGE,
    SESSION_ENVIRONMENT_TIER: ENGINE,
    SESSION_DEPLOYMENT_STORAGE_DIR: `${TMP}/storage`,
    AWAKEN_SCENARIO_SKILL_ID: 'delivered-container',
    AWAKEN_ACP_ARGV: `node -e ${ACP_FIXTURE}`,
    // Exercise the production pool wrapper. Resource-bearing environments are
    // deliberately non-poolable, so this changes composition without creating a
    // second Session container or weakening the one-environment assertion below.
    AWAKEN_SANDBOX_WARM_POOL: '1',
    // Disable the reaper's periodic sweep noise during the short test; the startup
    // sweep still runs (proving it is harmless with no leaked containers present).
    AWAKEN_SANDBOX_REAP_INTERVAL: '3600',
    AWAKEN_CONTAINER_FORWARD_PROXY: 'http://127.0.0.1:9',
  };
  const spawnBrain = (overrides = {}) => spawn(bin, {
    env: { ...brainEnv, ...overrides },
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  let brain = spawnBrain();

  try {
    // Readiness cause/effect table: live child + bound process-scoped port =>
    // proceed; child exits first => fail immediately; neither before 60s =>
    // timeout. FMECA: a copied wall-clock poll could hide child failure or be
    // distorted by clock jumps, so every restart uses the canonical harness.
    await waitForPort(PORT, 60_000, brain);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
    if (process.env.AWAKEN_E2E_PACKAGE_ONLY === '1') {
      await client.beta.skills.create({
        files: [await toFile(Buffer.from(
          '---\nname: delivered-container\ndescription: package e2e skill\nenvironment: filesystem\n---\nPACKAGE-E2E-SKILL',
        ), 'SKILL.md')],
      });
      const registryOnly = process.env.AWAKEN_E2E_PACKAGE_REGISTRY_ONLY === '1';
      await exercisePackageManagerMatrix(client, { registryOnly });
      if (!registryOnly) {
        const stopped = new Promise((resolve) => brain.once('exit', resolve));
        brain.kill('SIGINT');
        await stopped;
        brain = spawnBrain({ AWAKEN_SCENARIO_PLAYWRIGHT_MCP: '1' });
        await waitForPort(PORT, 60_000, brain);
        client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
        await exerciseContainerEnvironment(client, `${ENGINE}-playwright-mcp`, {
          environment: { kind: 'image', reference: PACKAGE_BASE_IMAGE },
        }, true, {
          packages: {
            apt: ['chromium'],
            npm: ['@playwright/mcp@0.0.78'],
          },
          expectedMarker: 'AWAKEN-PLAYWRIGHT-MCP-OK',
          proofCommands: [
            'test -x /usr/bin/chromium',
            'test -x /usr/local/bin/playwright-mcp',
          ],
        });
        console.log('  ok: Environment-installed Chromium served a browser through local stdio Playwright MCP');

        const nativeStopped = new Promise((resolve) => brain.once('exit', resolve));
        brain.kill('SIGINT');
        await nativeStopped;
        brain = spawnBrain({ AWAKEN_SCENARIO_NATIVE_PLAYWRIGHT_MCP: '1' });
        await waitForPort(PORT, 60_000, brain);
        client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
        await exerciseContainerEnvironment(client, `${ENGINE}-native-playwright-mcp`, {
          environment: { kind: 'image', reference: PACKAGE_BASE_IMAGE },
        }, true, {
          packages: {
            apt: ['chromium'],
            npm: ['@playwright/mcp@0.0.78'],
          },
          expectedMarker: 'AWAKEN-NATIVE-PLAYWRIGHT-MCP-OK',
          proofCommands: [
            'test -x /usr/bin/chromium',
            'test -x /usr/local/bin/playwright-mcp',
          ],
        });
        console.log('  ok: Native Runtime drove the sandbox Playwright MCP over its attached stdio channel');
      }
      console.log(
        registryOnly
          ? `E2E PASS: identical Managed Environment packages reuse one registry-backed immutable ${ENGINE} image across isolated Sessions.`
          : `E2E PASS: all six package managers plus local stdio Playwright MCP work in the ${ENGINE} sandbox.`,
      );
      return;
    }
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('CONTAINER-FILE-OK'), 'input.txt'),
      betas: BETAS,
    });
    // Memory projection cause/effect graph:
    // C1 container tier + C2 host FUSE available -> E1 force portable copy-bind;
    // C3 seeded store -> E2 hydrate bytes before launch; C4 writable agent edit +
    // C5 Session release -> E3 harvest once into durable store. C4 without C5
    // must not make a read API acquire the release side effect.
    //
    // | Rule | container | host FUSE | seed | edit | release | expected effect        |
    // | M1   | yes       | yes/none  | yes  | no   | no      | create + hydrate       |
    // | M2   | yes       | yes/none  | yes  | yes  | yes     | harvest durable edit   |
    // | M3   | yes       | yes/none  | yes  | yes  | no      | no read-side harvest   |
    // M1 is observed by the in-container seed assertion; M2 by the post-delete
    // repository assertion. The delete boundary between them excludes M3.
    const memory = await client.post('/v1/memory_stores', {
      body: { name: 'container-agent-memory' },
      headers: MEMORY_HEADERS,
    });
    await client.post(`/v1/memory_stores/${memory.id}/memories`, {
      body: { path: '/seed.txt', content: 'CONTAINER-MEMORY-SEED' },
      headers: MEMORY_HEADERS,
    });
    const deliveredSkillRecord = await client.beta.skills.create({
      files: [await toFile(Buffer.from(
        '---\nname: delivered-container\ndescription: delivered container skill\nenvironment: filesystem\n---\nCONTAINER-DELIVERED-SKILL-OK',
      ), 'SKILL.md')],
    });

    const session = await client.beta.sessions.create({
      agent: 'namespace-agent',
      environment_id: 'env_local',
      resources: [
        { type: 'file', file_id: file.id, mount_path: '/workspace/input.txt' },
        { type: 'memory_store', memory_store_id: memory.id, mount_path: '/notes' },
        { type: 'github_repository', url: skillRepository, mount_path: '/workspace/skills' },
      ],
      betas: BETAS,
    });
    // Main container Run rule: C1 Session owns File+Memory+Repository+Skill;
    // C2 exact User receipt is admitted; C3 ACP container commits MARKER after
    // C2. Effects: one later Agent message and one shared Session container.
    // Constraints/invariant: only events following the exact processed receipt
    // can prove this Run and all resources stay inside that Session container.
    const initialReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'run the containerized agent' }] }],
      betas: BETAS,
    });
    const initialAcceptedId = initialReceipt.data[0]?.id;
    assert.equal(typeof initialAcceptedId, 'string', 'main container Run exact User Event receipt');
    const { events } = await waitForSessionEventReceipt(
      client,
      session.id,
      initialAcceptedId,
      BETAS,
      ({ delta }) => delta.some(
        (event) => event.type === 'agent.message'
          && (event.content ?? []).some((content) => String(content.text ?? '').includes(MARKER)),
      ),
      'the main container Run to round-trip its Agent marker',
      { timeoutMs: 180_000, pollMs: 100 },
    );

    const messages = events
      .filter((e) => e.type === 'agent.message')
      .map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
    assert.ok(
      messages.some((m) => m.includes(MARKER)),
      `the containerized agent's reply must round-trip to the SDK: ${JSON.stringify(events)}`,
    );

    const containers = testContainers();
    assert.equal(containers.length, 1, 'the Session must own one shared container, not one per attempt');
    const container = containers[0];
    assert.equal(
      execFileSync(ENGINE, ['exec', container, 'cat', '/mnt/session/uploads/workspace/input.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-FILE-OK',
      'the uploaded file must be materialized into the Session container',
    );
    assert.match(
      execFileSync(ENGINE, ['exec', container, 'cat', '/workspace/skills/greet/SKILL.md'], {
        encoding: 'utf8',
      }),
      /CONTAINER-SKILL-OK/,
      'the repository-backed workspace skill must be imported into the Session container',
    );
    const deliveredSkill = `/workspace/.skills/${deliveredSkillRecord.id}/SKILL.md`;
    assert.match(
      execFileSync(ENGINE, ['exec', container, 'cat', deliveredSkill], { encoding: 'utf8' }),
      /CONTAINER-DELIVERED-SKILL-OK/,
      'the durable delivered-skill bundle must be materialized into the Session container',
    );
    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -w "$1"',
      'awaken-skill-check',
      deliveredSkill,
    ]);
    const memorySeed = spawnSync(
      ENGINE,
      ['exec', container, 'cat', '/mnt/notes/seed.txt'],
      { encoding: 'utf8' },
    );
    const workspaceFiles = memorySeed.status === 0
      ? ''
      : execFileSync(
          ENGINE,
          ['exec', container, 'find', '/workspace', '-maxdepth', '4', '-type', 'f', '-print'],
          { encoding: 'utf8' },
        );
    assert.equal(
      memorySeed.status,
      0,
      `the governed memory filesystem must hydrate into the Session container; files=${workspaceFiles}`,
    );
    assert.equal(memorySeed.stdout, 'CONTAINER-MEMORY-SEED');

    const liveFile = await client.beta.files.upload({
      file: await toFile(Buffer.from('CONTAINER-LIVE-FILE-OK'), 'live.txt'),
      betas: BETAS,
    });
    const fileResource = await afterPendingActivation(() => client.beta.sessions.resources.add(session.id, {
      type: 'file',
      file_id: liveFile.id,
      mount_path: '/workspace/live.txt',
      betas: BETAS,
    }));
    assert.equal(
      execFileSync(ENGINE, ['exec', container, 'cat', '/mnt/session/uploads/workspace/live.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-LIVE-FILE-OK',
      'a live file attach must update the resident container workspace',
    );
    await afterPendingActivation(() => client.beta.sessions.resources.delete(fileResource.id, {
      session_id: session.id,
      betas: BETAS,
    }));
    const renamedFileResource = await afterPendingActivation(() =>
      client.beta.sessions.resources.add(session.id, {
        type: 'file',
        file_id: liveFile.id,
        mount_path: '/workspace/renamed.txt',
        betas: BETAS,
      }));
    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -e /mnt/session/uploads/workspace/live.txt',
    ]);
    assert.equal(
      execFileSync(ENGINE, ['exec', container, 'cat', '/mnt/session/uploads/workspace/renamed.txt'], {
        encoding: 'utf8',
      }),
      'CONTAINER-LIVE-FILE-OK',
      'delete + add moves a live File through the official subresource operations',
    );

    await afterPendingActivation(() => client.beta.sessions.resources.delete(renamedFileResource.id, {
      session_id: session.id,
      betas: BETAS,
    }));
    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'test ! -e /mnt/session/uploads/workspace/live.txt && test ! -e /mnt/session/uploads/workspace/renamed.txt',
    ]);

    // A hard brain crash must retain the Session-owned environment. The replacement
    // process restores the durable binding, adopts the exact same container, renews
    // its lease and reconnects both the hand and ACP process before another Run.
    const crashed = new Promise((resolve) => brain.once('exit', resolve));
    brain.kill('SIGKILL');
    await crashed;
    assert.deepEqual(testContainers(), [container], 'a brain crash must not reap the Session container');
    brain = spawnBrain();
    await waitForPort(PORT, 60_000, brain);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://${addr}` });
    // Crash-adoption rule: C1 prior marker exists; C2 replacement adopts the
    // exact container; C3 new exact receipt is admitted. Only a marker after C3
    // proves the replacement ACP path; the prior marker is ineligible.
    const resumedReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'resume the adopted container' }] }],
      betas: BETAS,
    });
    const resumedAcceptedId = resumedReceipt.data[0]?.id;
    assert.equal(typeof resumedAcceptedId, 'string', 'adopted container Run exact User Event receipt');
    const { events: resumedEvents } = await waitForSessionEventReceipt(
      client,
      session.id,
      resumedAcceptedId,
      BETAS,
      ({ delta }) => delta.some(
        (event) => event.type === 'agent.message'
          && (event.content ?? []).some((content) => String(content.text ?? '').includes(MARKER)),
      ),
      'the adopted container Run to commit a new Agent marker',
      { timeoutMs: 180_000, pollMs: 100 },
    );
    assert.ok(
      resumedEvents.some(
        (event) => event.type === 'agent.message'
          && (event.content ?? []).some((content) => String(content.text ?? '').includes(MARKER)),
      ),
      `the replacement brain must resume through the adopted container: ${JSON.stringify(resumedEvents)}`,
    );
    assert.deepEqual(
      testContainers(),
      [container],
      'replacement must adopt the same container instead of creating an attempt-local duplicate',
    );

    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'printf %s CONTAINER-ARTIFACT-OK > /mnt/session/outputs/result.txt',
    ]);

    execFileSync(ENGINE, [
      'exec',
      container,
      'sh',
      '-c',
      'printf %s CONTAINER-MEMORY-OK > /mnt/notes/container.txt',
    ]);
    // Copy-backed MemoryRepository mounts reconcile only at the Session's terminal
    // release edge. Read-only resource APIs must never acquire this write side effect.
    // Output cause/effect decision table: O1 live Session + output present => no
    // Files API side effect; O2 terminal release + output present => harvest before
    // sandbox disposal; O3 retry of the same terminal release => same content-id,
    // no duplicate File. This E2E covers O2 and relies on the Rust idempotency test
    // for O3; querying only after delete keeps the read plane side-effect free (O1).
    await client.beta.sessions.delete(session.id, { betas: BETAS });
    await waitForTestContainersToBeReaped();
    // O2 also requires the endpoint-specific Beta selector: GA Files has no
    // scope_id and must not become a parallel Session-artifact query path.
    const artifacts = await client.beta.files.list({ scope_id: session.id, betas: BETAS });
    const artifact = artifacts.data.find((entry) => entry.filename === 'result.txt');
    assert.ok(artifact, `terminal release must harvest container output: ${JSON.stringify(artifacts)}`);
    const artifactContent = await client.beta.files.download(artifact.id, { betas: BETAS });
    assert.equal(await artifactContent.text(), 'CONTAINER-ARTIFACT-OK');
    const harvested = await client.get(`/v1/memory_stores/${memory.id}/memories?view=full`, {
      headers: MEMORY_HEADERS,
    });
    assert.equal(
      harvested.data.find((entry) => entry.path === '/container.txt')?.content,
      'CONTAINER-MEMORY-OK',
      'container memory writes must reconcile through the host at Session release',
    );

    if (ENGINE === 'podman') {
      await exercisePodmanRootfsMatrix(client);
      console.log('  ok: Managed environments drive Podman package/image/private-root/network/limit behavior');
    } else {
      // Docker and Podman share the exact neutral package contract and immutable
      // content-addressed image behavior. Docker does not implement Podman's
      // private-root variants, so only the portable OCI-image package row runs here.
      await exerciseContainerEnvironment(client, 'docker-package-image', {
        environment: { kind: 'image', reference: PACKAGE_BASE_IMAGE },
      }, true, {
        packages: { npm: ['cowsay@1.6.0'] },
        proveImageReuse: true,
        proofCommands: ['cowsay AWAKEN | grep AWAKEN'],
      });
      console.log('  ok: Managed environments drive Docker immutable package-image behavior');
    }

    console.log(
      `E2E PASS: container agent — ACP, hand, file, memory, repository, workspace skill and immutable delivered skill shared one Session-owned ${ENGINE} environment.`,
    );
  } finally {
    brain.kill('SIGINT');
    if (!process.env.AWAKEN_E2E_KEEP_TMP) cleanupTestContainers();
    fs.rmSync(`/tmp/awaken-acp-container-${brain.pid}`, { recursive: true, force: true });
    if (!process.env.AWAKEN_E2E_KEEP_TMP) fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
