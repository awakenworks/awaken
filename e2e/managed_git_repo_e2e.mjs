// github_repository session resource end-to-end (ADR-0038).
//
// A repo mounted on a session is cloned into the sandbox HOST-SIDE (the token never
// enters the jail), the agent reads, edits, and commits in the working tree, and the
// Repository realizer publishes those commits at Session release. This proves:
// clone → agent reads the cloned file → agent writes/commits → release publishes
// back → a later session re-clones and sees the pushed change (durable in the remote,
// across a real process restart).
//
// Cause graph:
// create-time Repository checkout -> frozen exact config -> provision plan
// -> host clone/checkout -> sandbox-visible tree -> terminal publish.
//
// Decision table:
// | Rule | Checkout | Expected sandbox tree | Fallback |
// | G1 | omitted | remote default branch | none |
// | G2 | branch name | named branch HEAD | never default |
// | G3 | commit SHA | exact historical commit | never branch HEAD |
//
// Deterministic: the remote is a LOCAL bare git repo (no network, no real GitHub),
// and the `git-repo` model reads `workspace/repo/README.md` then writes NEW.txt.
//
// Run: (from e2e/)  node managed_git_repo_e2e.mjs
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  pass,
  startUpstream,
  realServerEnv,
  hasEndTurn,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38217);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const TMP = path.join(os.tmpdir(), `awaken-gitrepo-e2e-${process.pid}`);
const README = 'SEED_README_CONTENT_7742';
const FEATURE_README = 'FEATURE_BRANCH_CONTENT_5521';
const FEATURE_LATEST = 'FEATURE_BRANCH_LATEST_9981';
const MARKER = 'AGENT_REPO_MARKER_3390'; // must match GitRepoModel

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
const git = (args, cwd) => execFileSync('git', args, { cwd, encoding: 'utf8' });

// A bare repo (the "remote") seeded with one commit, returned as a filesystem path
// usable as a git clone URL.
function seedRemote() {
  const work = `${TMP}/seed`;
  fs.mkdirSync(work, { recursive: true });
  git(['init', '-q'], work);
  git(['symbolic-ref', 'HEAD', 'refs/heads/main'], work);
  git(['config', 'user.email', 'seed@t'], work);
  git(['config', 'user.name', 'seed'], work);
  fs.writeFileSync(`${work}/README.md`, README);
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed'], work);
  // A second branch whose README differs, so a `checkout: {branch}` mount is
  // provably distinct from the default branch.
  git(['checkout', '-q', '-b', 'feature'], work);
  fs.writeFileSync(`${work}/README.md`, FEATURE_README);
  git(['commit', '-q', '-am', 'feature readme'], work);
  const pinnedCommit = git(['rev-parse', 'HEAD'], work).trim();
  fs.writeFileSync(`${work}/README.md`, FEATURE_LATEST);
  git(['commit', '-q', '-am', 'advance feature'], work);
  git(['checkout', '-q', 'main'], work);
  const bare = `${TMP}/remote.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return { bare, pinnedCommit };
}

// Artifact projection rule: C4 exact Session scope + C5 Files beta selector ->
// E4 read-only catalog observation. K4 GA Files has no scope_id, and neither
// projection may publish Repository changes. R4 C4&&C5->E4.
async function listArtifacts(sid) {
  try {
    await client.beta.files.list({ scope_id: sid, betas: BETAS });
  } catch {
    /* ignore */
  }
}

// Drive a session: read the cloned README (proving the clone), write NEW.txt (parks
// for approval), then reply. Returns the joined text of all tool results.
async function driveRepoSession(bare, checkout) {
  const branch = checkout?.name ?? 'main';
  const remoteHeadBefore = git(['rev-parse', branch], bare).trim();
  const repo = { type: 'github_repository', url: bare, mount_path: '/workspace/repo' };
  if (checkout) repo.checkout = checkout;
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    resources: [repo],
    betas: BETAS,
  });
  const projected = session.resources.find((resource) => resource.type === 'github_repository');
  assert.ok(projected?.id, 'Repository projection is addressable');
  assert.deepEqual(projected.checkout ?? null, checkout ?? null, 'wire projection preserves checkout');
  // G0 lifecycle rule: C1=exact task receipt; C2=requires_action with qualified
  // pending ids; C3=exact allow batch; C4=end_turn. Effects: E1=each state edge
  // is receipt-scoped; E2=terminal repository work. Constraint: only the test
  // sends approvals; the observer never drives. Rules C1+C2=>approve once;
  // C1+C2+C3&&!C4=>next boundary; C1+C2+C3+C4=>E1+E2.
  const taskReceipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'work on the repo' }] }],
    betas: BETAS,
  });
  let observation = await waitForSessionEventReceipt(
    client,
    session.id,
    taskReceipt.data[0]?.id,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
    'G0 task reaches its first committed boundary',
    { timeoutMs: 30_000 },
  );
  let evs = observation.events;
  for (let boundary = 0; boundary < 10 && !hasEndTurn(evs); boundary += 1) {
    const idle = [...observation.delta]
      .reverse()
      .find((event) => event.type === 'session.status_idle');
    assert.equal(idle?.stop_reason?.type, 'requires_action', 'G0 nonterminal boundary requires approval');
    const pendingIds = idle.stop_reason.event_ids;
    assert.ok(pendingIds.length > 0, 'G0 requires_action names exact pending Event ids');
    const decisions = pendingIds.map((id) => {
      const toolUse = evs.find((event) => event.id === id && event.type === 'agent.tool_use');
      assert.equal(toolUse?.evaluated_permission, 'ask', `G0 ${id} is a gated tool call`);
      return { type: 'user.tool_confirmation', tool_use_id: id, result: 'allow' };
    });
    const approval = await client.beta.sessions.events.send(session.id, {
      events: decisions,
      betas: BETAS,
    });
    const receiptIds = approval.data.map((event) => event.id);
    observation = await waitForSessionEventReceipt(
      client,
      session.id,
      receiptIds.at(-1),
      BETAS,
      ({ events, delta }) => receiptIds.every((id) => events.some(
        (event) => event.id === id && event.processed_at,
      )) && delta.some((event) => event.type === 'session.status_idle'),
      'G0 approval batch reaches its next committed boundary',
      { timeoutMs: 30_000 },
    );
    evs = observation.events;
  }
  assert.ok(hasEndTurn(evs), 'G0 repository Run reaches end_turn within ten approval boundaries');
  await listArtifacts(session.id);
  assert.equal(
    git(['rev-parse', branch], bare).trim(),
    remoteHeadBefore,
    'Files GET does not advance the Repository remote',
  );
  await client.beta.sessions.delete(session.id, { betas: BETAS });
  const toolText = JSON.stringify(evs.filter((e) => e.type === 'agent.tool_result').map((e) => e.content));
  return { id: session.id, toolText };
}

async function main() {
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  const { bare, pinnedCommit } = seedRemote();
  const servers = [];
  const upstream = await startUpstream('gitRepo');
  try {
    // ---- server A: clone → agent reads/writes/commits → release publishes ----
    const a = spawnServer('git-repo', PORT, realServerEnv('gitRepo', upstream, { mode: 'git-repo' }));
    servers.push(a.server);
    await waitForPort(PORT);

    const s1 = await driveRepoSession(bare);
    assert.ok(s1.toolText.includes(README), `agent read the host-cloned repo file: ${s1.toolText}`);
    pass('repo cloned host-side; agent read the seeded file from the jail');

    // ---- checkout: {type:"branch"} clones that ref, not the default branch ----
    const sf = await driveRepoSession(bare, { type: 'branch', name: 'feature' });
    assert.ok(
      sf.toolText.includes(FEATURE_LATEST),
      `checkout:{branch:"feature"} put the feature README in the jail: ${sf.toolText}`,
    );
    assert.ok(!sf.toolText.includes(README), 'the feature checkout did not clone the default branch');
    pass('checkout:{type:"branch"} mounts the requested ref');

    // ---- checkout: {type:"commit"} is an exact historical tree pin --------
    const sc = await driveRepoSession(bare, { type: 'commit', sha: pinnedCommit });
    assert.ok(
      sc.toolText.includes(FEATURE_README),
      `checkout:{commit:${pinnedCommit}} exposed the historical tree: ${sc.toolText}`,
    );
    assert.ok(!sc.toolText.includes(FEATURE_LATEST), 'commit checkout never fell forward to branch HEAD');
    pass('checkout:{type:"commit"} mounts the exact historical commit');

    // The release boundary published the agent's own NEW.txt commit.
    const pushed = git(['show', `main:NEW.txt`], bare).trim();
    assert.equal(pushed, MARKER, 'host pushed the agent edit back to the remote');
    pass('Session release published the Agent-authored commit; Files GET stayed read-only');

    // ---- restart: a fresh process re-clones the remote and sees the pushed change ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('git-repo', PORT, realServerEnv('gitRepo', upstream, { mode: 'git-repo' }));
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
    upstream.close();
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
