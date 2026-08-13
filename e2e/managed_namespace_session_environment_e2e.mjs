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
  sendAndListNewEvents,
  spawnServer,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38172);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const SKILL_HEADERS = { 'anthropic-beta': 'skills-2025-10-02' };
const TMP = path.join(os.tmpdir(), `awaken-namespace-session-e2e-${process.pid}`);
const TIER = process.env.SESSION_ENVIRONMENT_TIER ?? 'namespace';
const AGENT = 'namespace-agent';

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
readline.createInterface({ input: process.stdin }).on('line', () => {
  // The reserved path is the sole output boundary across Workdir/Namespace/
  // Container; a cwd-relative directory is ordinary workspace state.
  const outputs = process.env.AWAKEN_OUTPUTS_DIR;
  fs.mkdirSync(outputs, { recursive: true });
  fs.writeFileSync(outputs + '/result.txt', 'NAMESPACE-ARTIFACT-OK');
  const observations = [
    ['skill', read('.skills/delivered-namespace/SKILL.md')],
    ['memory', read('.mnt/notes/seed.txt')],
    ['memory_writable', writable('.mnt/notes/seed.txt')],
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
  const events = await sendAndListNewEvents(client, sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
    betas: BETAS,
  });
  const replies = events.filter((event) => event.type === 'agent.message');
  const newEventTypes = events.map((event) => event.type);
  assert.ok(
    replies.length > 0,
    `the namespace agent emitted a new reply for ${JSON.stringify(prompt)}; new events: ${JSON.stringify(newEventTypes)}`,
  );
  return (replies.at(-1).content ?? []).map((item) => item.text ?? '').join('');
}

async function main() {
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
    AWAKEN_SCENARIO_SKILL_ID: 'delivered-namespace',
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
    const memory = await client.post('/v1/memory_stores', { headers: MEMORY_HEADERS });
    await client.post(`/v1/memory_stores/${memory.id}/memories`, {
      body: { path: '/seed.txt', content: 'NAMESPACE-MEMORY-OK' },
      headers: MEMORY_HEADERS,
    });
    const createdSkill = await client.post('/v1/skills', {
      headers: SKILL_HEADERS,
      body: {
        id: 'delivered-namespace',
        content: '---\ndescription: namespace skill\nenvironment: filesystem\n---\nNAMESPACE-SKILL-OK',
      },
    });
    assert.equal(createdSkill.id, 'delivered-namespace');
    const listedSkills = await client.get('/v1/skills', { headers: SKILL_HEADERS });
    assert.ok(
      listedSkills.data.some((skill) => skill.id === 'delivered-namespace'),
      'the Skill is visible in the runtime catalog before Session creation',
    );

    // Cause graph: fixed ACP test route -> scenario AgentConfigSource binding ->
    // plain Session reference -> ACP execution. Session metadata is deliberately
    // absent because metadata is descriptive and cannot select a backend.
    //
    // Decision table:
    // projected acp:custom | agent reference || fixed ACP route
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
    assert.ok(
      !fs.existsSync(sandboxRoot),
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
