// The combined resource/skill chain in ONE conversation (native backend), over the
// real provider wire. A single session configures a memory_store + a github_repository
// resource via the API, is offered a skill, and has out-of-band memory extraction —
// then one natural-language turn drives the whole ADR-0038/0036 loop:
//   configure (API) → apply (sandbox mounts) → NL turn → read and use a skill →
//   write the memory store → commit the repo → export patch evidence →
//   retrieve artifacts → apply/test/scan in an external worktree; remote push stays manual.
// Plus: the extractor sub-run saves a cross-session memory after the turn.
//
// The model runs for real (GenaiExecutor → fake upstream reproducing the `fullChain`
// scenario on the wire); the far ends (git remote = a local bare repo, memory store,
// blob store) are all real. Deterministic, hermetic, no API key.
//
// End-to-end FMECA / cause-effect graph (the assertions in `main` own the table):
// C1 authoritative Skill/Memory/Repository configuration exists; C2 Session
// freezes those resources and env_local; C3 the real provider turn completes;
// C4 gated mutations are approved; C5 archive/reconciliation succeeds; C6 exported
// evidence verifies and applies externally. Effects: E1 attached and repository-local
// Skills are announced and read; E2 Memory publishes while the Repository remote is unchanged;
// E3 patch, typed manifest and output Files have exact bytes; E4 extraction
// persists cross-Session memory; E5 equal authored Skill bytes are idempotent and
// changed bytes append one version. Any missing cause must fail the scenario,
// never be interpreted as an empty store or successful no-op.
//
// | Rule | C1 | C2 | C3 | C4 | C5 | Required effects |
// | F1 | yes | yes | yes | yes | yes | E1+E2+E3+E4+E5+C6 |
// | F2 | missing/invalid | any | any | any | any | fail closed before invocation |
// | F3 | yes | yes | provider/turn fails | any | any | no fabricated terminal success |
// | F4 | yes | yes | yes | denied | any | no gated Resource mutation |
// | F5 | yes | yes | yes | yes | fails | no false publication success; retryable intent remains |
// Negative partitions F2-F5 are exercised at their authoritative unit/integration
// boundaries; F1 is the non-redundant real-process composition proof here.
//
// Run: (from e2e/)  node managed_full_chain_e2e.mjs

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38221);
const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const SKILL_HEADERS = { 'anthropic-beta': 'skills-2025-10-02' };
const SKILL_BETAS = ['skills-2025-10-02'];
const TMP = path.join(os.tmpdir(), `awaken-fullchain-e2e-${process.pid}`);
const STORE_DIR = `${TMP}/storage`;
const README = 'SEED_README_FULLCHAIN';
const REPO_MARKER = 'REPO_FULLCHAIN_8830'; // must match the fullChain behavior
const MEMO_MARKER = 'MEMO_FULLCHAIN_5521';
const ARTIFACT_MARKER = 'ARTIFACT_FULLCHAIN_9142';
const REPOSITORY_SKILL_MARKER = 'REPOSITORY-SKILL-FULLCHAIN-3017';
const LATE_REPOSITORY_SKILL_MARKER = 'LATE-REPOSITORY-SKILL-FULLCHAIN-4819';

const client = () => new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const git = (args, cwd) => execFileSync('git', args, { cwd, encoding: 'utf8' });

// A bare repo (the "remote") seeded with one commit; its filesystem path is a clone URL.
function seedRemote() {
  const work = `${TMP}/seed`;
  fs.mkdirSync(work, { recursive: true });
  git(['init', '-q'], work);
  git(['symbolic-ref', 'HEAD', 'refs/heads/main'], work);
  git(['config', 'user.email', 'seed@t'], work);
  git(['config', 'user.name', 'seed'], work);
  fs.writeFileSync(`${work}/README.md`, README);
  fs.writeFileSync(
    `${work}/verify.sh`,
    '#!/usr/bin/env bash\nset -euo pipefail\ntest "$(cat CHAIN.txt)" = "REPO_FULLCHAIN_8830"\ngit diff --check\n',
    { mode: 0o755 },
  );
  fs.mkdirSync(`${work}/.claude/skills/repository-guide`, { recursive: true });
  fs.writeFileSync(
    `${work}/.claude/skills/repository-guide/SKILL.md`,
    `---\nname: repository-guide\ndescription: repository-local guidance\n---\n${REPOSITORY_SKILL_MARKER}`,
  );
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed'], work);
  const bare = `${TMP}/remote.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

function addLateRepositorySkill(bare) {
  const work = `${TMP}/late-skill`;
  git(['clone', '-q', bare, work]);
  git(['config', 'user.email', 'late@t'], work);
  git(['config', 'user.name', 'late'], work);
  fs.mkdirSync(`${work}/.claude/skills/late-guide`, { recursive: true });
  fs.writeFileSync(
    `${work}/.claude/skills/late-guide/SKILL.md`,
    `---\nname: late-guide\ndescription: added after a Session snapshot\n---\n${LATE_REPOSITORY_SKILL_MARKER}`,
  );
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'add late repository skill'], work);
  git(['push', '-q', 'origin', 'HEAD:main'], work);
}

const listEvents = async (c, sid) => {
  const evs = [];
  for await (const ev of c.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(ev);
  return evs;
};

// Release every gated (`ask`) tool call not yet approved — each `write` awaits for a
// confirmation, so approving lets the run advance to its terminal message.
async function approveGated(c, sid, evs, approved, receiptIds) {
  for (const e of evs) {
    if (e.type === 'agent.tool_use' && e.evaluated_permission === 'ask' && !approved.has(e.id)) {
      approved.add(e.id);
      // Intermediate permission admission is intentionally not a terminal
      // oracle: the owning prompt receipt below gates the whole turn after all
      // confirmations. This send only releases C4 and the loop observes state.
      const response = await c.beta.sessions.events.send(sid, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
        betas: BETAS,
      });
      receiptIds.push(response.data[0].id);
    }
  }
}

// Artifact projection rule: C8 exact Session scope + C9 Files beta selector ->
// E8 read-only catalog observation. K8 GA Files has no scope_id; reverse
// resource operations remain owned by replacement/release. R8 C8&&C9->E8.
async function listArtifacts(c, sid) {
  try {
    return await c.beta.files.list({ scope_id: sid, betas: BETAS });
  } catch {
    return null;
  }
}

async function probeRepositorySkillPaths(c, sid) {
  const receipt = (await c.beta.sessions.events.send(sid, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: 'probe-repository-skill-snapshot' }],
    }],
    betas: BETAS,
  })).data[0];
  const { delta } = await waitForSessionEventReceipt(
    c,
    sid,
    receipt.id,
    BETAS,
    ({ delta: events }) => events.some((event) => event.type === 'agent.message'),
    'repository Skill metadata probe to complete',
    { timeoutMs: 30_000, pollMs: 200 },
  );
  const reply = delta
    .filter((event) => event.type === 'agent.message')
    .flatMap((event) => event.content ?? [])
    .map((content) => content.text ?? '')
    .at(-1);
  assert.equal(typeof reply, 'string', 'probe returned one Agent text message');
  return JSON.parse(reply);
}

async function main() {
  cleanupFixtureTree(TMP);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const bare = seedRemote();
  const upstream = await startUpstream('fullChain');
  const { server } = spawnServer('full-chain', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    ...realServerEnv('fullChain', upstream, { mode: 'full-chain' }),
    SESSION_DEPLOYMENT_SANDBOX_TIER: 'namespace',
  });
  try {
    await waitForPort(PORT);
    const c = client();

    // 1) Configure resources via their authoritative APIs. The immutable Agent
    // publication selects `greet`; therefore the durable Skill aggregate must
    // exist before Session pinning (a static side registry is not a second truth).
    const greet = await c.beta.skills.create({
      display_title: 'Greet',
      files: [
        await toFile(
          Buffer.from('---\nname: greet\ndescription: greet\n---\nGREETING-FROM-SKILL'),
          'SKILL.md',
        ),
      ],
      betas: SKILL_BETAS,
    });
    assert.ok(greet.id.startsWith('skill_'));
    const mem = await c.post('/v1/memory_stores', {
      body: { name: 'full-chain-memory' },
      headers: MEMORY_HEADERS,
    });
    assert.ok(mem.id, 'POST /v1/memory_stores returned an id');
    pass(`configured a memory_store via the API: ${mem.id}`);

    // 2) One session binds BOTH resources; the skill is offered by the host.
    const session = await c.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    const skillIds = (session.agent.skills ?? []).map((s) => s.skill_id ?? s);
    assert.ok(skillIds.includes(greet.id), `the skill is offered: ${JSON.stringify(session.agent.skills)}`);
    assert.equal(session.resources.length, 2, 'both resources bound to the session');
    const remoteHeadBefore = git(['rev-parse', 'main'], bare).trim();
    pass('one session binds memory_store + github_repository and is offered the skill');

    // 3) A single natural-language turn drives the whole chain.
    // Receipt rule F6: C6 exact full-chain prompt receipt; E6 that receipt is
    // processed only with the terminal `done` effect. K1 old history and C4
    // intermediate approvals cannot complete F1. Decision F6=F1+C6=>E1-E6.
    const chainReceipt = (await c.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'do the full chain' }] }],
      betas: BETAS,
    })).data[0];

    // Drive await→approve until the turn ends. Artifact GET remains read-only.
    const approved = new Set();
    const approvalReceiptIds = [];
    let evs = [];
    let files = null;
    let memContent = '';
    for (let i = 0; i < 60; i += 1) {
      await sleep(400);
      evs = await listEvents(c, session.id);
      await approveGated(c, session.id, evs, approved, approvalReceiptIds);
      files = await listArtifacts(c, session.id);
      const done = evs.some((e) => e.type === 'agent.message' && (e.content ?? []).some((b) => (b.text ?? '').includes('done')));
      if (done) break;
    }
    ({ events: evs } = await waitForSessionEventReceipt(
      c,
      session.id,
      chainReceipt.id,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message'
        && (event.content ?? []).some((content) => (content.text ?? '').includes('done'))),
      'full-chain prompt to process into its terminal done message',
      { timeoutMs: 30_000, pollMs: 200 },
    ));
    for (const receiptId of approvalReceiptIds) {
      await waitForSessionEventReceipt(
        c,
        session.id,
        receiptId,
        BETAS,
        () => true,
        'full-chain permission receipt to process',
        { timeoutMs: 30_000, pollMs: 200 },
      );
    }
    const failedTools = evs.filter((event) => event.type === 'agent.tool_result' && event.is_error);
    assert.deepEqual(failedTools, [], `every full-chain tool effect succeeds: ${JSON.stringify(failedTools)}`);

    assert.equal(
      git(['rev-parse', 'main'], bare).trim(),
      remoteHeadBefore,
      'Files GET does not advance the Repository remote',
    );
    const archivedMain = await c.beta.sessions.archive(session.id, { betas: BETAS });
    assert.equal(archivedMain.status, 'terminated', 'main Session archive is the synchronous harvest edge');
    for (let i = 0; i < 60; i += 1) {
      // Observation decision table: full+success exposes durable bytes; basic
      // deliberately elides them; transport/decode failure is a test failure,
      // never evidence that the store is merely empty.
      const page = await c.get(`/v1/memory_stores/${mem.id}/memories?view=full`, {
        headers: MEMORY_HEADERS,
      });
      memContent = (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
      files = await listArtifacts(c, session.id);
      const listed = files?.data ?? files?.files ?? files ?? [];
      const listedArray = Array.isArray(listed) ? listed : listed.data ?? [];
      const names = listedArray.map((file) => file.filename ?? file.path ?? file.logical_path ?? '');
      if (
        memContent.includes(MEMO_MARKER)
        && ['result.txt', 'change.patch', 'manifest.json']
          .every((name) => names.some((listedName) => listedName.endsWith(name)))
      ) break;
      await sleep(200);
    }

    // Artifact handoff decision table: C1 a sandbox commit exists; C2 archive
    // harvested all three outputs; C3 the manifest authenticates the patch; C4 the
    // external checkout is still at the recorded base; C5 an operator explicitly
    // selects a review ref. Effects: E1 main stays unchanged; E2 `git apply
    // --check` succeeds; E3 apply changes only CHAIN.txt; E4 repository test and
    // diff scan pass; E5 only the explicit review ref receives the accepted
    // commit. Negative rules: changed bytes fail hash validation, base drift
    // fails the base guard, and a conflicting tree fails apply --check.
    const artifacts = files?.data ?? files?.files ?? files ?? [];
    const arr = Array.isArray(artifacts) ? artifacts : artifacts.data ?? [];
    const artifactNamed = (name) => arr.find((file) =>
      (file.filename ?? file.path ?? file.logical_path ?? '').endsWith(name));
    const download = async (name) => {
      const artifact = artifactNamed(name);
      assert.ok(artifact, `${name} is listed by /v1/files: ${JSON.stringify(arr)}`);
      const response = await fetch(`http://127.0.0.1:${PORT}/v1/files/${artifact.id}/content`, {
        headers: {
          'x-api-key': 'e2e-dummy',
          'anthropic-beta': BETAS.join(','),
        },
      });
      const bytes = Buffer.from(await response.arrayBuffer());
      assert.equal(response.status, 200, `${name} is downloadable: ${bytes.toString('utf8')}`);
      return bytes;
    };
    const resultBytes = await download('result.txt');
    const patchBytes = await download('change.patch');
    const manifestBytes = await download('manifest.json');
    assert.equal(resultBytes.toString('utf8'), ARTIFACT_MARKER, 'sandbox output bytes are exact');
    const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
    const manifest = JSON.parse(manifestBytes.toString('utf8'));
    assert.equal(manifest.schema, 'awaken.repository_patch.v1', 'manifest has one typed contract');
    assert.equal(manifest.base_commit, remoteHeadBefore, 'manifest records the mounted baseline');
    assert.notEqual(manifest.sandbox_commit, remoteHeadBefore, 'manifest records the sandbox commit');
    assert.equal(manifest.patch_sha256, sha256(patchBytes), 'manifest binds the exact patch bytes');
    assert.equal(git(['rev-parse', 'main'], bare).trim(), remoteHeadBefore, 'archive does not publish the sandbox commit');

    // Negative partitions are checked before the successful application so
    // none can borrow success from the happy-path worktree.
    const tamperedPatch = Buffer.concat([patchBytes, Buffer.from('\n# tampered')]);
    assert.notEqual(sha256(tamperedPatch), manifest.patch_sha256, 'changed patch bytes fail the integrity check');
    const drifted = `${TMP}/drifted`;
    git(['clone', '-q', bare, drifted]);
    git(['config', 'user.email', 'operator@t'], drifted);
    git(['config', 'user.name', 'operator'], drifted);
    fs.appendFileSync(`${drifted}/README.md`, '\nlocal drift\n');
    git(['add', 'README.md'], drifted);
    git(['commit', '-q', '-m', 'local drift'], drifted);
    assert.notEqual(git(['rev-parse', 'HEAD'], drifted).trim(), manifest.base_commit, 'drifted base fails the manifest guard');
    const conflicting = `${TMP}/conflicting`;
    git(['clone', '-q', bare, conflicting]);
    fs.writeFileSync(`${conflicting}/CHAIN.txt`, 'conflicting local bytes');
    const patchPath = `${TMP}/change.patch`;
    fs.writeFileSync(patchPath, patchBytes);
    assert.notEqual(
      spawnSync('git', ['apply', '--check', patchPath], { cwd: conflicting, stdio: 'ignore' }).status,
      0,
      'conflicting worktree fails apply --check',
    );

    const acceptance = `${TMP}/acceptance`;
    git(['clone', '-q', bare, acceptance]);
    assert.equal(git(['rev-parse', 'HEAD'], acceptance).trim(), manifest.base_commit, 'external checkout matches manifest base');
    git(['apply', '--check', patchPath], acceptance);
    git(['apply', patchPath], acceptance);
    assert.equal(fs.readFileSync(`${acceptance}/CHAIN.txt`, 'utf8'), REPO_MARKER, 'external worktree received exact change');
    assert.deepEqual(git(['status', '--porcelain'], acceptance).trim().split('\n'), ['?? CHAIN.txt'], 'only the intended file changed');
    execFileSync('bash', ['verify.sh'], { cwd: acceptance, stdio: 'pipe' });
    git(['diff', '--check'], acceptance);
    git(['config', 'user.email', 'operator@t'], acceptance);
    git(['config', 'user.name', 'operator'], acceptance);
    git(['add', 'CHAIN.txt'], acceptance);
    git(['commit', '-q', '-m', 'operator: accept full-chain patch'], acceptance);
    const acceptedCommit = git(['rev-parse', 'HEAD'], acceptance).trim();
    assert.equal(git(['rev-parse', 'HEAD^'], acceptance).trim(), manifest.base_commit, 'operator commit is based on the manifest-bound baseline');
    git(['push', '-q', 'origin', 'HEAD:refs/heads/review/session-full-chain'], acceptance);
    assert.equal(git(['rev-parse', 'main'], bare).trim(), remoteHeadBefore, 'operator push never mutates main');
    assert.equal(git(['rev-parse', 'review/session-full-chain'], bare).trim(), acceptedCommit, 'only the selected review ref receives the operator commit');
    assert.equal(git(['show', 'review/session-full-chain:CHAIN.txt'], bare), REPO_MARKER, 'review ref contains exact accepted bytes');
    pass('patch evidence failed tamper/drift/conflict cases, then applied, tested, scanned, and was explicitly pushed to a review ref');

    // 4) Attached and repository-local Skills share the sole filesystem path.
    // Causes: C6 the Session has filesystem tools and one frozen attached Skill;
    // C7 the mounted repository contains exact
    // `.claude/skills/<name>/SKILL.md`; C8 `read` is enabled. Effects: E6 both
    // metadata entries are announced and each body arrives through ordinary
    // `read`; E7 neither semantic Skill tool is exposed or called.
    // Decision rule F6=C6+C7+C8=>E6+E7. Startup timing and same-name repository
    // combinations remain owned by the lower-level decision-table test.
    const toolNames = evs.filter((e) => e.type === 'agent.tool_use').map((e) => e.name);
    assert.deepEqual(toolNames.slice(0, 2), ['read', 'read'], 'both Skill bodies use ordinary read');
    assert.ok(
      !toolNames.includes('list_skills') && !toolNames.includes('Skill'),
      `filesystem delivery has no semantic parallel path: ${JSON.stringify(toolNames)}`,
    );
    const skillResults = JSON.stringify(
      evs.filter((event) => event.type === 'agent.tool_result').map((event) => event.content),
    );
    assert.ok(skillResults.includes('GREETING-FROM-SKILL'), 'attached Skill body reached read result');
    assert.ok(
      skillResults.includes(REPOSITORY_SKILL_MARKER),
      'repository-local Skill body reached read result from its discovered path',
    );
    pass('attached + repository-local Skills were discovered and read through one filesystem path');

    // Repository Skill startup snapshot decision table:
    // C9=read enabled, C10=Session has already completed its first discovery,
    // C11=remote gains a Skill after that snapshot, C12=new Session, C13=read
    // disabled while bash remains enabled. E8=old Session stays frozen; E9=new
    // Session sees both repository paths; E10=read-disabled Session receives no
    // repository metadata even though it still has a filesystem tool.
    // R7 C9+C10 => original only; R8 R7+C11 => E8; R9 C9+C11+C12 => E9;
    // R10 C11+C12+C13 => E10. The probe returns prompt metadata without reading
    // bodies, so tool success cannot fabricate discovery evidence.
    const frozenRepositorySession = await c.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    const frozenBefore = await probeRepositorySkillPaths(c, frozenRepositorySession.id);
    assert.deepEqual(
      frozenBefore,
      ['/workspace/repo/.claude/skills/repository-guide/SKILL.md'],
      'R7 first Run freezes the original repository Skill metadata',
    );
    addLateRepositorySkill(bare);
    const frozenAfter = await probeRepositorySkillPaths(c, frozenRepositorySession.id);
    assert.deepEqual(frozenAfter, frozenBefore, 'R8 current Session keeps its startup snapshot');

    const freshRepositorySession = await c.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    assert.deepEqual(
      await probeRepositorySkillPaths(c, freshRepositorySession.id),
      [
        '/workspace/repo/.claude/skills/late-guide/SKILL.md',
        '/workspace/repo/.claude/skills/repository-guide/SKILL.md',
      ],
      'R9 a new Session snapshots the advanced repository checkout',
    );

    const readDisabledRepositorySession = await c.beta.sessions.create({
      agent: {
        id: 'assistant',
        type: 'agent_with_overrides',
        tools: [{
          type: 'agent_toolset_20260401',
          default_config: {
            enabled: false,
            permission_policy: { type: 'always_allow' },
          },
          configs: [{
            name: 'bash',
            enabled: true,
            permission_policy: { type: 'always_allow' },
          }],
        }],
      },
      environment_id: 'env_local',
      resources: [
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    assert.deepEqual(
      await probeRepositorySkillPaths(c, readDisabledRepositorySession.id),
      [],
      'R10 repository Skill discovery requires read, not merely bash',
    );
    for (const probeSession of [
      frozenRepositorySession,
      freshRepositorySession,
      readDisabledRepositorySession,
    ]) {
      const archived = await c.beta.sessions.archive(probeSession.id, { betas: BETAS });
      assert.equal(archived.status, 'terminated', 'probe Session cleanup reached its terminal edge');
    }
    pass('repository Skill discovery is startup-scoped and gated by the read capability');

    // 5) Memory store write-back landed.
    assert.ok(memContent.includes(MEMO_MARKER), `memory-store write-back landed: ${JSON.stringify(memContent)}`);
    pass('Session release reconciled memory_store write under its id');

    // 6) Cross-session memory: the extractor sub-run saved a memory the store persisted.
    // Extraction is out-of-band (fires after the turn's terminal step, runs its own
    // model call), so poll the durable memory root while the server is still up.
    const grep = (dir, needle) => {
      if (!fs.existsSync(dir)) return false;
      for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
        const p = `${dir}/${e.name}`;
        if (e.isDirectory()) {
          if (grep(p, needle)) return true;
        } else if (fs.readFileSync(p, 'utf8').includes(needle)) {
          return true;
        }
      }
      return false;
    };
    let extracted = false;
    for (let i = 0; i < 25 && !extracted; i += 1) {
      await sleep(400);
      extracted = grep(STORE_DIR, 'full chain ran');
    }
    assert.ok(extracted, 'out-of-band extraction persisted a memory to the durable store');
    pass('out-of-band memory extraction saved a cross-session memory');

    // The same production sandbox can author a Skill under `skills/<id>/SKILL.md`.
    // Harvest persists it into the canonical SkillStore: identical bytes are a no-op,
    // changed bytes append exactly one immutable version, and later Sessions see it.
    const author = async (prompt) => {
      const authored = await c.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
        betas: BETAS,
      });
      // Authoring rule A1: C1 exact authoring receipt and C2 gated writes release;
      // E1 processed receipt with the matching authored marker. K1 a marker from
      // another Session/version cannot satisfy this turn. D1=C1+C2=>E1.
      const authoredReceipt = (await c.beta.sessions.events.send(authored.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
        betas: BETAS,
      })).data[0];
      const approved = new Set();
      const approvalReceiptIds = [];
      let authoredEvents = [];
      for (let attempt = 0; attempt < 30; attempt += 1) {
        await sleep(200);
        authoredEvents = await listEvents(c, authored.id);
        await approveGated(c, authored.id, authoredEvents, approved, approvalReceiptIds);
        if (authoredEvents.some((event) =>
          event.type === 'agent.message' &&
          (event.content ?? []).some((content) => (content.text ?? '').includes(`authored ${prompt}`)),
        )) break;
      }
      ({ events: authoredEvents } = await waitForSessionEventReceipt(
        c,
        authored.id,
        authoredReceipt.id,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message'
          && (event.content ?? []).some((content) => (content.text ?? '').includes(`authored ${prompt}`))),
        `authored Skill turn ${prompt}`,
        { timeoutMs: 30_000, pollMs: 200 },
      ));
      for (const receiptId of approvalReceiptIds) {
        await waitForSessionEventReceipt(
          c,
          authored.id,
          receiptId,
          BETAS,
          () => true,
          `authored Skill permission receipt for ${prompt}`,
          { timeoutMs: 30_000, pollMs: 200 },
        );
      }
      assert.ok(
        authoredEvents.some((event) =>
          event.type === 'agent.message' &&
          (event.content ?? []).some((content) => (content.text ?? '').includes(`authored ${prompt}`)),
        ),
      );
      // Archive is the synchronous terminal edge: it returns only after Skill
      // harvest and sandbox cleanup settle. Delete intentionally detaches the
      // same durable cleanup, so a 404 is not a completion receipt.
      const archived = await c.beta.sessions.archive(authored.id, { betas: BETAS });
      assert.equal(archived.status, 'terminated');
    };
    let authoredSkillId = '';
    const skillVersions = async () => {
      const response = await c.get(`/v1/skills/${authoredSkillId}/versions`, {
        headers: SKILL_HEADERS,
      });
      return response.data ?? [];
    };
    await author('author-skill-v1');
    const authoredCatalog = await c.get('/v1/skills', { headers: SKILL_HEADERS });
    authoredSkillId = (authoredCatalog.data ?? [])
      .find((skill) => skill.id !== greet.id)?.id ?? '';
    assert.ok(authoredSkillId, 'terminal harvest published the authored Skill aggregate');
    assert.deepEqual((await skillVersions()).map((version) => version.version), ['1']);
    await author('author-skill-v1');
    assert.deepEqual(
      (await skillVersions()).map((version) => version.version),
      ['1'],
      're-harvesting identical SKILL.md bytes is idempotent',
    );
    await author('author-skill-v2');
    assert.deepEqual((await skillVersions()).map((version) => version.version), ['1', '2']);
    const authoredLatest = await c.get(`/v1/skills/${authoredSkillId}/versions/latest`, {
      headers: SKILL_HEADERS,
    });
    assert.equal(authoredLatest.version, '2');
    assert.match(
      await (await fetch(`http://127.0.0.1:${PORT}/v1/skills/${authoredSkillId}/versions/2/content`, {
        headers: SKILL_HEADERS,
      })).text(),
      /AUTHORED_SKILL_V2/u,
    );
    const consumingSession = await c.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'memory_store', memory_store_id: mem.id, mount_path: '/memory' }],
      betas: BETAS,
    });
    const selectedSkills = (consumingSession.agent.skills ?? []).map((skill) => skill.skill_id ?? skill);
    assert.deepEqual(
      selectedSkills,
      [greet.id],
      'persisting an authored Skill does not mutate the immutable Agent publication',
    );
    const skillCatalog = await c.get('/v1/skills', { headers: SKILL_HEADERS });
    assert.ok(
      (skillCatalog.data ?? []).some((skill) => skill.id === authoredSkillId),
      'the authored aggregate remains available for an explicit future publication update',
    );
    const consumingArchived = await c.beta.sessions.archive(consumingSession.id, { betas: BETAS });
    assert.equal(consumingArchived.status, 'terminated');
    pass('agent-authored Skill versions persist without implicitly mutating Agent selection');

    console.log('E2E PASS: full chain — config → mounts → skill → memory + sandbox commit → verified patch handoff → authored Skill versions.');
  } finally {
    await stopServer(server);
    upstream.close();
    // F5 cleanup uses the same mount-aware owner as every durable Memory
    // fixture; logical Session deletion and physical teardown are intentionally
    // separate, so raw recursive deletion is not a valid cleanup oracle.
    cleanupFixtureTree(TMP);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
