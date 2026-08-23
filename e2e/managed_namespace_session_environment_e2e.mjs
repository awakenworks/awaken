// Managed API -> one Session-owned local/namespace environment.
//
// This is the namespace sibling of managed_container_agent_e2e.mjs. It drives the
// explicit dev/test ACP launch composition and mutates resources only after the
// first turn has made the Session environment live. The fixture observes the same
// workspace across turns, proving that attach/update/detach changes one governed
// projection instead of creating an attempt-local sandbox.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  onlyChildDirectory,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38172);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const SKILL_HEADERS = { 'anthropic-beta': 'skills-2025-10-02' };
const TMP = path.join(os.tmpdir(), `awaken-namespace-session-e2e-${process.pid}`);
const TIER = process.env.SESSION_ENVIRONMENT_TIER ?? 'namespace';
const AGENT = 'namespace-agent';

async function waitUntil(predicate, message, attempts = 200) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  assert.fail(message);
}

function bwrapAvailable() {
  return spawnSync('bwrap', ['--unshare-user', '--ro-bind', '/', '/', '--', 'true'], {
    stdio: 'ignore',
  }).status === 0;
}

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8' });
}

function initMainRepository(work) {
  // Cause: Git <2.28 has no `git init -b`; effect: the fixture must still
  // expose the exact `main` branch identity on every developer platform.
  git(['init', '-q'], work);
  git(['symbolic-ref', 'HEAD', 'refs/heads/main'], work);
}

function seedRepository() {
  const work = `${TMP}/repo-seed`;
  fs.mkdirSync(work, { recursive: true });
  initMainRepository(work);
  git(['config', 'user.email', 'namespace-e2e@awaken.invalid'], work);
  git(['config', 'user.name', 'Awaken Namespace E2E'], work);
  fs.writeFileSync(`${work}/README.md`, 'NAMESPACE-REPOSITORY-OK');
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed namespace repository'], work);
  const bare = `${TMP}/repository.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

function seedAgentFixtureRepository() {
  const work = `${TMP}/fixture-seed`;
  const memoryPath = TIER === 'namespace' ? '/mnt/notes/seed.txt' : 'mnt/notes/seed.txt';
  fs.mkdirSync(work, { recursive: true });
  initMainRepository(work);
  git(['config', 'user.email', 'namespace-e2e@awaken.invalid'], work);
  git(['config', 'user.name', 'Awaken Namespace E2E'], work);
  fs.writeFileSync(
    `${work}/namespace-agent.mjs`,
    `import fs from 'node:fs';
import readline from 'node:readline';
const read = (path) => { try { return fs.readFileSync(path, 'utf8'); } catch { return 'ABSENT'; } };
const readAny = (...paths) => paths.map(read).find((value) => value !== 'ABSENT') ?? 'ABSENT';
const readProjectedSkill = (...roots) => {
  for (const root of roots) {
    try {
      for (const entry of fs.readdirSync(root).sort()) {
        const content = read(root + '/' + entry + '/SKILL.md');
        if (content !== 'ABSENT') return content;
      }
    } catch {}
  }
  return 'ABSENT';
};
const writable = (path) => { try { fs.accessSync(path, fs.constants.W_OK); return true; } catch { return false; } };
const mutateExisting = (path) => {
  try {
    if (!fs.existsSync(path)) return false;
    fs.writeFileSync(path, 'SESSION-COPY-MODIFIED');
    return true;
  } catch { return false; }
};
// Multi-turn fixture decision table:
// R1 one input line -> one observation + one turn_end.
// R2 N sequential input lines -> N fresh observations, each reflecting the
//    resident workspace at that turn (never reuse the first turn's bytes).
// R3 partial line -> no turn until the newline framing contract is complete.
// Constraint: this newline protocol is a scenario-only fixture, not production ACP.
//
// Memory path decision table (C1=tier has sandbox path fidelity; C2=the mount
// is read-only): local (!C1,!C2) -> read the Workdir-root-relative projection
// and observe writable=true; namespace (C1,C2) -> read the exact absolute
// /mnt contract path and observe writable=false. Any other path must remain a
// hard test failure; no fallback may hide a broken provider projection.
const memoryPath = ${JSON.stringify(memoryPath)};
readline.createInterface({ input: process.stdin }).on('line', () => {
  // The reserved path is the sole output boundary across Workdir/Namespace/
  // Container; a cwd-relative directory is ordinary workspace state.
  const outputs = process.env.AWAKEN_OUTPUTS_DIR;
  fs.mkdirSync(outputs, { recursive: true });
  fs.writeFileSync(outputs + '/result.txt', 'NAMESPACE-ARTIFACT-OK');
  const observations = [
    ['skill', readProjectedSkill('.skills', 'workspace/.skills')],
    ['memory', read(memoryPath)],
    ['memory_writable', fs.existsSync(memoryPath) && writable(memoryPath)],
    ['live_file', read('/mnt/session/uploads/workspace/live.txt')],
    ['live_file_mutated', mutateExisting('/mnt/session/uploads/workspace/live.txt')],
    ['renamed_file', read('/mnt/session/uploads/workspace/renamed.txt')],
    ['renamed_file_mutated', mutateExisting('/mnt/session/uploads/workspace/renamed.txt')],
    ['live_repo', readAny('live-repo/README.md', 'workspace/live-repo/README.md')],
    ['renamed_repo', readAny('renamed-repo/README.md', 'workspace/renamed-repo/README.md')],
  ];
  console.log(JSON.stringify({ type: 'message', text: JSON.stringify(observations) }));
  console.log(JSON.stringify({ type: 'turn_end', reason: 'natural_end' }));
});
`,
  );
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed namespace agent'], work);
  const bare = `${TMP}/fixture.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

async function lastReply(client, sessionId, prompt) {
  // Namespace Run settlement rule N1: C1=the Session can already contain prior
  // replies/idle edges; C2=this send returns one exact durable User Event
  // receipt; C3=ACP commits the corresponding Run asynchronously. Effects:
  // E1=only events after C2 are eligible; E2=C2 becomes processed; E3=one new
  // Agent Message and later Session idle prove settlement. N1(C1+C2+C3)->E1-E3.
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
    betas: BETAS,
  });
  const acceptedId = receipt.data[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'N1 exact accepted User Event id');
  const { delta: events } = await waitForSessionEventReceipt(
    client,
    sessionId,
    acceptedId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    `N1 namespace Run for ${JSON.stringify(prompt)} to settle`,
    { timeoutMs: 120_000, pollMs: 100 },
  );
  const replies = events.filter((event) => event.type === 'agent.message');
  const newEventTypes = events.map((event) => event.type);
  assert.ok(
    replies.length > 0,
    `the namespace agent emitted a new reply for ${JSON.stringify(prompt)}; new events: ${JSON.stringify(newEventTypes)}`,
  );
  return (replies.at(-1).content ?? []).map((item) => item.text ?? '').join('');
}

async function main() {
  // Causes: C1 the selected tier is Local or available Namespace; C2 a first
  // Run realizes the Session environment; C3 File/Repository bindings are
  // rejected, attached, replaced, or detached across later Runs; C4 Namespace
  // execution restarts after a hard process loss. Effects: E1 all Runs reuse one
  // workspace; E2 every accepted generation exposes only its current paths and
  // access policy; E3 C4 adopts the same durable environment; E4 terminal delete
  // releases it. Constraints/invariants: the Session baseline and Resource
  // generations are the only projection authorities; tier-specific fixture
  // paths cannot create an attempt-local or fallback mount truth. Decision rules:
  // N0 unavailable Namespace=>skip; N1 C1+C2=>E1; N2 N1+C3=>E1+E2;
  // N3 N2+C4=>E3; N4 terminal delete=>E4.
  assert.ok(['local', 'namespace'].includes(TIER), `unsupported Session environment tier: ${TIER}`);
  if (TIER === 'namespace' && !bwrapAvailable()) {
    console.log('E2E SKIP: bwrap/unprivileged userns unavailable on this host.');
    return;
  }
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  const repository = seedRepository();
  const fixtureRepository = seedAgentFixtureRepository();
  const home = `${TMP}/home`;
  fs.mkdirSync(`${home}/.awaken`, { recursive: true });
  fs.writeFileSync(`${home}/.awaken/config.toml`, [
    `data_dir = ${JSON.stringify(`${TMP}/storage`)}`,
    `sandbox_tier = ${JSON.stringify(TIER)}`,
    `sandbox_dir = ${JSON.stringify(`${TMP}/sandboxes`)}`,
    '',
  ].join('\n'));
  const serverEnv = {
    HOME: home,
    // Scenario-only fixed-launch composition still consumes these explicit test
    // knobs; the production CLI independently resolves the same values from TOML.
    SESSION_ENVIRONMENT_TIER: TIER,
    SESSION_DEPLOYMENT_SANDBOX_DIR: `${TMP}/sandboxes`,
    SESSION_DEPLOYMENT_STORAGE_DIR: `${TMP}/storage`,
    // JSON preserves Windows executable and fixture paths containing spaces;
    // the scenario host still accepts the legacy whitespace-delimited form.
    AWAKEN_ACP_ARGV: JSON.stringify(TIER === 'namespace'
      ? [process.execPath, '/workspace/fixture/namespace-agent.mjs']
      : [process.execPath, `${TMP}/fixture-seed/namespace-agent.mjs`]),
  };
  let running = spawnServer('acp-container', PORT, serverEnv);
  let server = running.server;
  let sandboxRoot;

  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: running.baseUrl });
    const memory = await client.post('/v1/memory_stores', {
      body: { name: 'namespace-session-memory' },
      headers: MEMORY_HEADERS,
    });
    await client.post(`/v1/memory_stores/${memory.id}/memories`, {
      body: { path: '/seed.txt', content: 'NAMESPACE-MEMORY-OK' },
      headers: MEMORY_HEADERS,
    });
    const createdSkill = await client.beta.skills.create({
      files: [await toFile(Buffer.from(
        '---\nname: delivered-namespace\ndescription: namespace skill\nenvironment: filesystem\n---\nNAMESPACE-SKILL-OK',
      ), 'SKILL.md')],
    });
    assert.ok(createdSkill.id.startsWith('skill_'));
    const listedSkills = await client.get('/v1/skills', { headers: SKILL_HEADERS });
    assert.ok(
      listedSkills.data.some((skill) => skill.id === createdSkill.id),
      'the Skill is visible in the runtime catalog before Session creation',
    );

    // Cause graph: fixed ACP test route -> scenario AgentConfigSource binding ->
    // plain Session reference -> ACP execution. Session metadata is deliberately
    // absent because metadata is descriptive and cannot select a backend.
    //
    // Decision table:
    // projected acp:claude | agent reference || fixed ACP test launch
    // no projection        | assistant       || host default (not this test)

    const session = await client.beta.sessions.create({
      agent: AGENT,
      environment_id: 'env_local',
      resources: [
        {
          type: 'memory_store',
          memory_store_id: memory.id,
          mount_path: '/notes',
          access: TIER === 'namespace' ? 'read_only' : 'read_write',
        },
        {
          type: 'github_repository',
          url: fixtureRepository,
          mount_path: '/workspace/fixture',
        },
        {
          type: 'github_repository',
          url: repository,
          mount_path: '/workspace/live-repo',
        },
      ],
      betas: BETAS,
    });
    let reply = await lastReply(client, session.id, 'observe initial namespace');
    assert.match(reply, /NAMESPACE-SKILL-OK/, 'the immutable Skill bundle reached the namespace');
    assert.match(reply, /NAMESPACE-MEMORY-OK/, 'the read-only governed memory reached the namespace');
    assert.match(
      reply,
      new RegExp(`memory_writable",${TIER === 'local'}`),
      `${TIER} enforces its declared memory access capability`,
    );
    sandboxRoot = onlyChildDirectory(
      `${TMP}/sandboxes`,
      'one Session-owned sandbox exists',
    );

    const uploaded = await client.beta.files.upload({
      file: await toFile(Buffer.from('NAMESPACE-FILE-OK'), 'live.txt'),
      betas: BETAS,
    });
    const repoResource = session.resources.find((resource) =>
      resource.type === 'github_repository' && resource.mount_path === '/workspace/live-repo');
    assert.ok(repoResource?.id);
    // Live File admission decision table:
    // read-only mount + namespace enforcement -> attach at the official
    // /mnt/session/uploads path; read-only mount + local Workdir -> reject before
    // changing the resident projection. Every admitted copy rejects mutation.
    if (TIER === 'local') {
      await assert.rejects(
        client.beta.sessions.resources.add(session.id, {
          type: 'file',
          file_id: uploaded.id,
          mount_path: '/workspace/live.txt',
          betas: BETAS,
        }),
        (error) => error?.status === 400 && /does not enforce read-only/.test(error.message),
      );
      reply = await lastReply(client, session.id, 'observe rejected live attachment');
      assert.match(reply, /live_file","ABSENT/, 'a rejected attach leaves no live path');
      assert.match(reply, /live_repo","NAMESPACE-REPOSITORY-OK/, 'the existing repository remains pinned');
    } else {
      const fileResource = await client.beta.sessions.resources.add(session.id, {
        type: 'file',
        file_id: uploaded.id,
        mount_path: '/workspace/live.txt',
        betas: BETAS,
      });
      reply = await lastReply(client, session.id, 'observe live attachments');
      assert.match(reply, /NAMESPACE-FILE-OK/, 'a live file attach changed the resident workspace');
      assert.match(reply, /live_file_mutated",false/, 'the mounted File copy is read-only');
      assert.match(reply, /NAMESPACE-REPOSITORY-OK/, 'the create-time repository remains pinned');

      await client.beta.sessions.resources.delete(fileResource.id, {
        session_id: session.id,
        betas: BETAS,
      });
      const renamedFileResource = await client.beta.sessions.resources.add(session.id, {
        type: 'file',
        file_id: uploaded.id,
        mount_path: '/workspace/renamed.txt',
        betas: BETAS,
      });
      reply = await lastReply(client, session.id, 'observe renamed attachments');
      assert.match(reply, /live_file","ABSENT/, 'the old live-file path was revoked');
      assert.match(reply, /renamed_file","NAMESPACE-FILE-OK/, 'the file appeared only at its replacement path');
      assert.match(reply, /renamed_file_mutated",false/, 'the replacement copy is also read-only');
      assert.equal(
        (await client.beta.files.retrieveMetadata(uploaded.id, { betas: BETAS })).id,
        uploaded.id,
        'the read-only Session projection leaves the logical input File live',
      );
      assert.match(reply, /live_repo","NAMESPACE-REPOSITORY-OK/, 'create-time repository remains pinned');

      await client.beta.sessions.resources.delete(renamedFileResource.id, {
        session_id: session.id,
        betas: BETAS,
      });
    }
    await client.beta.sessions.resources.delete(repoResource.id, {
      session_id: session.id,
      betas: BETAS,
    });
    reply = await lastReply(client, session.id, 'observe detached resources');
    assert.match(reply, /renamed_file","ABSENT/, 'file detach revoked the live path');
    assert.match(reply, /live_repo","ABSENT/, 'repository detach revoked its create-time path');

    if (TIER === 'namespace') {
      // A hard process crash leaves the durable SandboxHandle and namespace tree
      // behind. The replacement must adopt that exact environment before serving
      // the next turn; rebuilding an empty attempt-local sandbox would lose the
      // mounted fixture, Skill, memory and prior outputs.
      const crashed = new Promise((resolve) => server.once('exit', resolve));
      server.kill('SIGKILL');
      await crashed;
      assert.ok(
        fs.existsSync(sandboxRoot),
        'a process crash retains the namespace tree for durable adoption',
      );
      assert.ok(
        fs.existsSync(`${TMP}/storage/sessions.db`),
        'the frozen Session baseline was committed before the crash',
      );
      const durableSession = execFileSync(
        'sqlite3',
        [`${TMP}/storage/sessions.db`, `SELECT session_id FROM managed_session WHERE session_id = '${session.id}'`],
        { encoding: 'utf8' },
      ).trim();
      assert.equal(durableSession, session.id, 'the durable repository contains the Session row');
      running = spawnServer('acp-container', PORT, serverEnv);
      server = running.server;
      await waitForPort(PORT, 180_000, server);
      client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: running.baseUrl });
      reply = await lastReply(client, session.id, 'observe adopted namespace after crash');
      assert.match(reply, /NAMESPACE-SKILL-OK/, 'replacement process reused the delivered Skill tree');
      assert.match(reply, /NAMESPACE-MEMORY-OK/, 'replacement process reused the governed memory tree');
      assert.match(reply, /renamed_file","ABSENT/, 'replacement retained the detached resource state');
      assert.match(reply, /live_repo","ABSENT/, 'replacement did not recreate a detached repository');
    }

    const memoryResource = session.resources.find((resource) => resource.type === 'memory_store');
    assert.equal(memoryResource.id, undefined, 'Memory attachment has no mutable resource address');
    assert.ok(
      (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).resources
        .some((resource) => resource.type === 'memory_store' && resource.memory_store_id === memory.id),
      'the create-time Memory authority remains in the frozen Session after live mutations/restart',
    );

    const artifacts = await client.get(`/v1/files?scope_id=${session.id}`);
    const artifact = artifacts.data.find((entry) => entry.filename === 'result.txt');
    assert.ok(artifact, `${TIER} output must project through the Files API`);
    const artifactContent = await client.beta.files.download(artifact.id, { betas: BETAS });
    assert.equal(await artifactContent.text(), 'NAMESPACE-ARTIFACT-OK');

    await client.beta.sessions.delete(session.id, { betas: BETAS });
    await waitUntil(
      () => !fs.existsSync(sandboxRoot),
      `${TIER} terminal Session deletion disposes its retained environment`,
    );

    console.log(`E2E PASS: ${TIER} Session retained one sandbox across Skill/memory materialization, live resource changes${TIER === 'namespace' ? ', crash adoption' : ''}, and release.`);
  } finally {
    await stopServer(server);
    cleanupFixtureTree(TMP);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
