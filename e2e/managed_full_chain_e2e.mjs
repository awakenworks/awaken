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
// C4 gated mutations are approved; C5 archive publishes its durable terminal
// fence without implicitly publishing sandbox-local files; C6 exported
// evidence verifies and applies externally. Effects: E1 attached and repository-local
// Skills are announced and read; E2 Memory publishes while the Repository remote is unchanged;
// E3 patch, typed manifest and output Files have exact bytes; E4 extraction
// persists cross-Session memory; E5 an Agent-authored Skill is written and read
// back with exact bytes inside its originating Managed Session, while the explicit Skill
// catalog remains unchanged. Any missing cause must fail the scenario,
// never be interpreted as an empty store or successful no-op.
//
// | Rule | C1 | C2 | C3 | C4 | C5 | Required effects |
// | F1 | yes | yes | yes | yes | yes | E1+E2+E3+E4+E5+C6 |
// | F2 | missing/invalid | any | any | any | any | fail closed before invocation |
// | F3 | yes | yes | provider/turn fails | any | any | no fabricated terminal success |
// | F4 | yes | yes | yes | denied | any | no gated Resource mutation |
// | F5 | yes | yes | yes | yes | fails | no false cleanup success; retryable intent remains |
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
  FILES_BETA,
  allowManagedToolBoundaries,
  cleanupFixtureTree,
  hasEndTurn,
  pass,
  realServerEnv,
  scenarioMemoryStore,
  SKILLS_BETA,
  SKILLS_BETAS,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38221);
const BETAS = ['managed-agents-2026-04-01', FILES_BETA];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const SKILL_HEADERS = { 'anthropic-beta': SKILLS_BETA };
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
    ({ delta: events }) => events.some((event) => event.type === 'agent.message')
      && hasEndTurn(events),
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
      betas: SKILLS_BETAS,
    });
    assert.ok(greet.id.startsWith('skill_'));
    const mem = await scenarioMemoryStore(c, MEMORY_HEADERS);
    assert.ok(mem.id, 'the scenario Resources application published a real MemoryStore');
    pass(`resolved the Agent-bound memory_store through the API: ${mem.id}`);

    // 2) One session binds BOTH resources; the skill is offered by the host.
    const session = await c.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'memory_store', memory_store_id: mem.id },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    const sessionMemory = session.resources.find(
      (resource) => resource.type === 'memory_store' && resource.memory_store_id === mem.id,
    );
    assert.equal(
      typeof sessionMemory?.mount_path,
      'string',
      'full-chain Session returns its exact frozen MemoryStore mount',
    );
    const memoryNotePath = `${sessionMemory.mount_path}/note.md`;
    const skillIds = (session.agent.skills ?? []).map((s) => s.skill_id ?? s);
    assert.deepEqual(
      skillIds,
      [greet.id],
      `only the explicitly published Skill is offered: ${JSON.stringify(session.agent.skills)}`,
    );
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

    // The canonical harness owns await→approve→end_turn sequencing. Artifact
    // GET remains a read-only observation after that one driver completes.
    const evs = await allowManagedToolBoundaries({
      client: c,
      sessionId: session.id,
      taskReceiptId: chainReceipt.id,
      betas: BETAS,
      description: 'full-chain prompt',
      timeoutMs: 30_000,
      maxBoundaries: 20,
    });
    let files = await listArtifacts(c, session.id);
    let memContent = '';
    assert.ok(
      evs.some((event) => event.type === 'agent.message'
        && (event.content ?? []).some((content) => (content.text ?? '').includes('done'))),
      'the terminal full-chain message carries the scenario-owned done effect',
    );
    const failedTools = evs.filter((event) => event.type === 'agent.tool_result' && event.is_error);
    assert.deepEqual(failedTools, [], `every full-chain tool effect succeeds: ${JSON.stringify(failedTools)}`);

    // Memory coordinate decision: C1 Session create returns the frozen Memory
    // resource mount; C2 the provider request carries exactly one corresponding
    // frozen prompt (missing/multiple fail closed in the fixture); C3 the write
    // is emitted. E1 the tool path is exactly `${C1}/note.md`, not a copied or
    // re-derived catalog path.
    const memoryWrites = evs.filter(
      (event) => event.type === 'agent.tool_use' && event.name === 'write'
        && event.input?.path === memoryNotePath,
    );
    assert.equal(
      memoryWrites.length,
      1,
      `one Memory write uses the Session-returned path ${memoryNotePath}`,
    );

    assert.equal(
      git(['rev-parse', 'main'], bare).trim(),
      remoteHeadBefore,
      'Files GET does not advance the Repository remote',
    );
    const archivedMain = await c.beta.sessions.archive(session.id, { betas: BETAS });
    assert.equal(
      archivedMain.status,
      'terminated',
      'main Session archive publishes the durable terminal fence; cleanup is observed below',
    );
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

    // Artifact handoff decision table: C1 a sandbox commit exists; C2 the
    // post-archive lifecycle observation finds all three outputs; C3 the manifest
    // binds the exact patch bytes; C4 the
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
        { type: 'memory_store', memory_store_id: mem.id },
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
        { type: 'memory_store', memory_store_id: mem.id },
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
        { type: 'memory_store', memory_store_id: mem.id },
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

    // A Managed Session may author a canonical Skill file inside its own frozen
    // sandbox, but only an explicit publication API owns the Skill catalog.
    const author = async (prompt) => {
      const authored = await c.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'memory_store', memory_store_id: mem.id }],
        betas: BETAS,
      });
      assert.deepEqual(
        (authored.agent.skills ?? []).map((skill) => skill.skill_id ?? skill),
        [greet.id],
        'the authoring Session starts from the one explicitly published Agent Skill',
      );
      // Authoring decision table A1. Causes: C1 one originating Managed Session;
      // C2 gated write is approved; C3 ordinary read targets the same canonical
      // path; C4 the exact User receipt reaches the committed end_turn idle;
      // C5 archive returns its durable terminal fence. Effects: E1 one successful
      // write and read expose exact bytes inside that Session; E2 the Session
      // becomes terminated; E3 the Skill catalog gains no implicit entry. Rule
      // A1=C1+C2+C3+C4+C5=>E1+E2+E3. Missing/error/drifted C2-C4 must fail before
      // E1/E2; lower-level lifecycle tests own asynchronous cleanup retry and
      // no-promotion recovery.
      const authoredReceipt = (await c.beta.sessions.events.send(authored.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
        betas: BETAS,
      })).data[0];
      const authoredEvents = await allowManagedToolBoundaries({
        client: c,
        sessionId: authored.id,
        taskReceiptId: authoredReceipt.id,
        betas: BETAS,
        description: `authored Skill turn ${prompt}`,
        timeoutMs: 30_000,
      });
      assert.ok(
        authoredEvents.some((event) =>
          event.type === 'agent.message' &&
          (event.content ?? []).some((content) => (content.text ?? '').includes(`authored ${prompt}`)),
        ),
      );
      const authoredPath = 'skills/authored/SKILL.md';
      const writes = authoredEvents.filter((event) =>
        event.type === 'agent.tool_use'
          && event.name === 'write'
          && event.input?.path === authoredPath);
      const reads = authoredEvents.filter((event) =>
        event.type === 'agent.tool_use'
          && event.name === 'read'
          && event.input?.path === authoredPath);
      assert.equal(writes.length, 1, 'A1/E1 writes one canonical authored Skill file');
      assert.match(writes[0].input?.content ?? '', /AUTHORED_SKILL_V1/u, 'A1/E1 write bytes');
      assert.equal(reads.length, 1, 'A1/E1 reads the same canonical authored Skill file');
      const writeResults = authoredEvents.filter((event) =>
        event.type === 'agent.tool_result' && event.tool_use_id === writes[0].id);
      const readResults = authoredEvents.filter((event) =>
        event.type === 'agent.tool_result' && event.tool_use_id === reads[0].id);
      assert.equal(writeResults.length, 1, 'A1/E1 writes have one linked result');
      assert.equal(readResults.length, 1, 'A1/E1 reads have one linked result');
      const [writeResult] = writeResults;
      const [readResult] = readResults;
      assert.ok(writeResult && !writeResult.is_error, 'A1/E1 write completed successfully');
      assert.ok(readResult && !readResult.is_error, 'A1/E1 read completed successfully');
      const readText = (readResult.content ?? [])
        .filter((block) => block.type === 'text')
        .map((block) => block.text ?? '')
        .join('');
      assert.equal(
        readText,
        writes[0].input.content,
        'A1/E1 read result exactly equals the authored bytes',
      );
      // Archive commits the logical terminal fence; physical cleanup is an
      // independently retryable lifecycle and is not a Skill publication receipt.
      const archived = await c.beta.sessions.archive(authored.id, { betas: BETAS });
      assert.equal(archived.status, 'terminated', 'A1/E2 archive publishes the terminal fence');
    };
    await author('author-skill-v1');
    const authoredCatalog = await c.get('/v1/skills', { headers: SKILL_HEADERS });
    assert.deepEqual(
      (authoredCatalog.data ?? []).map((skill) => skill.id),
      [greet.id],
      'A1/E3 archive does not implicitly promote a sandbox-local authored file',
    );
    pass('one Managed Session authored and read a sandbox-local Skill file without implicit promotion');

    console.log('E2E PASS: full chain — config → mounts → skill → memory + sandbox commit → verified patch handoff → sandbox-local authored Skill.');
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
