// Managed API -> one Session-owned local/namespace environment.
//
// This is the namespace sibling of managed_container_agent_e2e.mjs. It drives the
// production `with_acp_from_env` composition and mutates resources only after the
// first turn has made the Session environment live. The fixture observes the same
// workspace across turns, proving that attach/update/detach changes one governed
// projection instead of creating an attempt-local sandbox.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { execFileSync, spawnSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38172);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const TMP = `/tmp/awaken-namespace-session-e2e-${process.pid}`;
const TIER = process.env.SESSION_ENVIRONMENT_TIER ?? 'namespace';

function bwrapAvailable() {
  return spawnSync('bwrap', ['--unshare-user', '--ro-bind', '/', '/', '--', 'true'], {
    stdio: 'ignore',
  }).status === 0;
}

function git(args, cwd) {
  return execFileSync('git', args, { cwd, encoding: 'utf8' });
}

function seedRepository() {
  const work = `${TMP}/repo-seed`;
  fs.mkdirSync(work, { recursive: true });
  git(['init', '-q', '-b', 'main'], work);
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
  git(['init', '-q', '-b', 'main'], work);
  git(['config', 'user.email', 'namespace-e2e@awaken.invalid'], work);
  git(['config', 'user.name', 'Awaken Namespace E2E'], work);
  fs.writeFileSync(
    `${work}/namespace-agent.mjs`,
    `import fs from 'node:fs';
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
process.stdin.once('data', () => {
  fs.mkdirSync('outputs', { recursive: true });
  fs.writeFileSync('outputs/result.txt', 'NAMESPACE-ARTIFACT-OK');
  const observations = [
    ['skill', read('.skills/delivered-namespace/SKILL.md')],
    ['memory', read('.mnt/notes/seed.txt')],
    ['memory_writable', writable('.mnt/notes/seed.txt')],
    ['live_file', read('.mnt/workspace/live.txt')],
    ['live_file_mutated', mutateExisting('.mnt/workspace/live.txt')],
    ['renamed_file', read('.mnt/workspace/renamed.txt')],
    ['renamed_file_mutated', mutateExisting('.mnt/workspace/renamed.txt')],
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
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  const replies = events.filter((event) => event.type === 'agent.message');
  assert.ok(replies.length > 0, 'the namespace agent emitted a reply');
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
  const serverEnv = {
    AWAKEN_SANDBOX_TIER: TIER,
    AWAKEN_SANDBOX_DIR: `${TMP}/sandboxes`,
    AWAKEN_ACP_ARGV: TIER === 'namespace'
      ? 'node /workspace/fixture/namespace-agent.mjs'
      : `node ${TMP}/fixture-seed/namespace-agent.mjs`,
    AWAKEN_STORAGE_DIR: `${TMP}/storage`,
  };
  let running = spawnServer('acp-container', PORT, serverEnv);
  let server = running.server;

  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: running.baseUrl });
    const memory = await client.post('/v1/memory_stores');
    await client.post(`/v1/memory_stores/${memory.id}/memories`, {
      body: { path: '/seed.txt', content: 'NAMESPACE-MEMORY-OK' },
    });
    const createdSkill = await client.post('/v1/skills', {
      body: {
        id: 'delivered-namespace',
        content: '---\ndescription: namespace skill\n---\nNAMESPACE-SKILL-OK',
      },
    });
    assert.equal(createdSkill.id, 'delivered-namespace');
    const listedSkills = await client.get('/v1/skills');
    assert.ok(
      listedSkills.data.some((skill) => skill.id === 'delivered-namespace'),
      'the Skill is visible in the runtime catalog before Session creation',
    );

    const session = await client.beta.sessions.create({
      agent: 'assistant',
      metadata: { 'awaken.runtime': 'acp:custom' },
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

    const uploaded = await client.beta.files.upload({
      file: await toFile(Buffer.from('NAMESPACE-FILE-OK'), 'live.txt'),
      betas: BETAS,
    });
    const fileResource = await client.beta.sessions.resources.add(session.id, {
      type: 'file',
      file_id: uploaded.id,
      mount_path: '/workspace/live.txt',
      betas: BETAS,
    });
    const repoResource = await client.beta.sessions.resources.add(session.id, {
      type: 'github_repository',
      url: repository,
      mount_path: '/workspace/live-repo',
      betas: BETAS,
    });
    reply = await lastReply(client, session.id, 'observe live attachments');
    assert.match(reply, /NAMESPACE-FILE-OK/, 'a live file attach changed the resident workspace');
    assert.match(reply, /live_file_mutated",true/, 'the Agent may edit its disposable File copy');
    assert.equal(
      await (await client.beta.files.download(uploaded.id, { betas: BETAS })).text(),
      'NAMESPACE-FILE-OK',
      'editing the Session copy cannot mutate the immutable FileStore object',
    );
    assert.match(reply, /NAMESPACE-REPOSITORY-OK/, 'a live repository attach changed the resident workspace');

    await client.beta.sessions.resources.update(fileResource.id, {
      session_id: session.id,
      mount_path: '/workspace/renamed.txt',
      betas: BETAS,
    });
    await client.beta.sessions.resources.update(repoResource.id, {
      session_id: session.id,
      mount_path: '/workspace/renamed-repo',
      betas: BETAS,
    });
    reply = await lastReply(client, session.id, 'observe renamed attachments');
    assert.match(reply, /live_file","ABSENT/, 'the old live-file path was revoked');
    assert.match(reply, /renamed_file","NAMESPACE-FILE-OK/, 'the file appeared only at its replacement path');
    assert.match(reply, /renamed_file_mutated",true/, 'the replacement is another disposable copy');
    assert.equal(
      await (await client.beta.files.download(uploaded.id, { betas: BETAS })).text(),
      'NAMESPACE-FILE-OK',
      'repeated copy mutation still leaves the content-addressed File unchanged',
    );
    assert.match(reply, /live_repo","ABSENT/, 'the old repository path was revoked');
    assert.match(reply, /renamed_repo","NAMESPACE-REPOSITORY-OK/, 'the repository was reprovisioned at its replacement path');

    await client.beta.sessions.resources.delete(fileResource.id, {
      session_id: session.id,
      betas: BETAS,
    });
    await client.beta.sessions.resources.delete(repoResource.id, {
      session_id: session.id,
      betas: BETAS,
    });
    reply = await lastReply(client, session.id, 'observe detached resources');
    assert.match(reply, /renamed_file","ABSENT/, 'file detach revoked the live path');
    assert.match(reply, /renamed_repo","ABSENT/, 'repository detach revoked the live path');

    if (TIER === 'namespace') {
      // A hard process crash leaves the durable SandboxHandle and namespace tree
      // behind. The replacement must adopt that exact environment before serving
      // the next turn; rebuilding an empty attempt-local sandbox would lose the
      // mounted fixture, Skill, memory and prior outputs.
      const crashed = new Promise((resolve) => server.once('exit', resolve));
      server.kill('SIGKILL');
      await crashed;
      assert.ok(
        fs.existsSync(`${TMP}/sandboxes/${session.id}`),
        'a process crash retains the namespace tree for durable adoption',
      );
      running = spawnServer('acp-container', PORT, serverEnv);
      server = running.server;
      await waitForPort(PORT, 180_000, server);
      client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: running.baseUrl });
      reply = await lastReply(client, session.id, 'observe adopted namespace after crash');
      assert.match(reply, /NAMESPACE-SKILL-OK/, 'replacement process reused the delivered Skill tree');
      assert.match(reply, /NAMESPACE-MEMORY-OK/, 'replacement process reused the governed memory tree');
      assert.match(reply, /renamed_file","ABSENT/, 'replacement retained the detached resource state');
      assert.match(reply, /renamed_repo","ABSENT/, 'replacement did not recreate a detached repository');
    }

    const memoryResource = session.resources.find((resource) => resource.type === 'memory_store');
    await assert.rejects(
      () => client.beta.sessions.resources.delete(memoryResource.id, {
        session_id: session.id,
        betas: BETAS,
      }),
      (error) => error.status === 400,
      'a live Session cannot detach its create-time memory authority',
    );

    const artifacts = await client.get(`/v1/files?scope_id=${session.id}`);
    const artifact = artifacts.data.find((entry) => entry.filename === 'result.txt');
    assert.ok(artifact, `${TIER} output must project through the Files API`);
    const artifactContent = await client.beta.files.download(artifact.id, { betas: BETAS });
    assert.equal(await artifactContent.text(), 'NAMESPACE-ARTIFACT-OK');

    await client.beta.sessions.delete(session.id, { betas: BETAS });
    assert.ok(
      !fs.existsSync(`${TMP}/sandboxes/${session.id}`),
      `${TIER} terminal Session deletion disposes its retained environment`,
    );

    console.log(`E2E PASS: ${TIER} Session retained one sandbox across Skill/memory materialization, live resource changes${TIER === 'namespace' ? ', crash adoption' : ''}, and release.`);
  } finally {
    await stopServer(server);
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
