// github_repository session resource end-to-end (ADR-0038).
//
// A repo mounted on a session is cloned into the sandbox HOST-SIDE (the token never
// enters the jail), the agent reads and edits the working tree, and the host commits
// + pushes the edits back to the remote on harvest. This proves the whole loop the
// old stub skipped: clone → agent reads the cloned file → agent writes → host pushes
// back → a later session re-clones and sees the pushed change (durable in the remote,
// across a real process restart).
//
// Deterministic: the remote is a LOCAL bare git repo (no network, no real GitHub),
// and the `git-repo` model reads `workspace/repo/README.md` then writes NEW.txt.
//
// Run: (from e2e/)  node managed_git_repo_e2e.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38217);
const BETAS = ['managed-agents-2026-04-01'];
const TMP = `/tmp/awaken-gitrepo-e2e-${process.pid}`;
const README = 'SEED_README_CONTENT_7742';
const MARKER = 'AGENT_REPO_MARKER_3390'; // must match GitRepoModel

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const git = (args, cwd) => execFileSync('git', args, { cwd, encoding: 'utf8' });

// A bare repo (the "remote") seeded with one commit, returned as a filesystem path
// usable as a git clone URL.
function seedRemote() {
  const work = `${TMP}/seed`;
  fs.mkdirSync(work, { recursive: true });
  git(['init', '-q', '-b', 'main'], work);
  git(['config', 'user.email', 'seed@t'], work);
  git(['config', 'user.name', 'seed'], work);
  fs.writeFileSync(`${work}/README.md`, README);
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed'], work);
  const bare = `${TMP}/remote.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

const listEvents = async (sid) => {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(ev);
  return evs;
};

async function approveGated(sid, evs, approved) {
  for (const e of evs) {
    if (e.type === 'agent.tool_use' && e.evaluated_permission === 'ask' && !approved.has(e.id)) {
      approved.add(e.id);
      await client.beta.sessions.events.send(sid, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
        betas: BETAS,
      });
    }
  }
}

// `GET /v1/files?scope_id` is the reverse-channel trigger: it also commits + pushes
// the session's repo edits back to the remote.
async function harvest(sid) {
  try {
    await client.get(`/v1/files?scope_id=${sid}`);
  } catch {
    /* ignore */
  }
}

// Drive a session: read the cloned README (proving the clone), write NEW.txt (parks
// for approval), then reply. Returns the joined text of all tool results.
async function driveRepoSession(bare) {
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    resources: [{ type: 'github_repository', url: bare, mount_path: '/workspace/repo' }],
    betas: BETAS,
  });
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'work on the repo' }] }],
    betas: BETAS,
  });
  const approved = new Set();
  let evs = [];
  for (let i = 0; i < 40; i += 1) {
    await sleep(400);
    evs = await listEvents(session.id);
    await approveGated(session.id, evs, approved);
    if (evs.some((e) => e.type === 'agent.message')) break;
  }
  await harvest(session.id);
  const toolText = JSON.stringify(evs.filter((e) => e.type === 'agent.tool_result').map((e) => e.content));
  return { id: session.id, toolText };
}

async function main() {
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  const bare = seedRemote();
  const servers = [];
  try {
    // ---- server A: clone → agent reads seed → agent writes → host pushes back ----
    const a = spawnServer('git-repo', PORT);
    servers.push(a.server);
    await waitForPort(PORT);

    const s1 = await driveRepoSession(bare);
    assert.ok(s1.toolText.includes(README), `agent read the host-cloned repo file: ${s1.toolText}`);
    pass('repo cloned host-side; agent read the seeded file from the jail');

    // The host committed + pushed the agent's NEW.txt back to the bare remote.
    const pushed = git(['show', `main:NEW.txt`], bare).trim();
    assert.equal(pushed, MARKER, 'host pushed the agent edit back to the remote');
    pass('host committed + pushed the agent edit back to the remote (write-back)');

    // ---- restart: a fresh process re-clones the remote and sees the pushed change ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('git-repo', PORT);
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    const s2 = await driveRepoSession(bare);
    assert.ok(s2.toolText.includes(README), 'a new session AFTER restart re-clones the repo');
    // The re-clone carries the previously-pushed file too (durable in the remote).
    const stillThere = git(['show', `main:NEW.txt`], bare).trim();
    assert.equal(stillThere, MARKER, 'the pushed change is durable in the remote across restart');
    pass('a later session re-clones the remote and the write-back survived a restart');

    console.log('E2E PASS: ADR-0038 github_repository clone + agent edit + host push-back.');
  } finally {
    for (const srv of servers) await stopServer(srv);
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
