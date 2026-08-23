// The combined resource/skill chain in ONE conversation (native backend), over the
// real provider wire. A single session configures a memory_store + a github_repository
// resource via the API, is offered a skill, and has out-of-band memory extraction —
// then one natural-language turn drives the whole ADR-0038/0036 loop:
//   configure (API) → apply (sandbox mounts) → NL turn → use a skill →
//   write the memory store → commit the repo → produce an output artifact →
//   retrieve the artifact via GET /v1/files → release once to reconcile/publish.
// Plus: the extractor sub-run saves a cross-session memory after the turn.
//
// The model runs for real (GenaiExecutor → fake upstream reproducing the `fullChain`
// scenario on the wire); the far ends (git remote = a local bare repo, memory store,
// blob store) are all real. Deterministic, hermetic, no API key.
//
// End-to-end FMECA / cause-effect graph (the assertions in `main` own the table):
// C1 authoritative Skill/Memory/Repository configuration exists; C2 Session
// freezes those resources and env_local; C3 the real provider turn completes;
// C4 gated mutations are approved; C5 release/reconciliation succeeds. Effects:
// E1 Skill is offered and invoked; E2 Memory and Repository mutations publish;
// E3 the output is one downloadable File whose bytes are exact; E4 extraction
// persists cross-Session memory; E5 equal authored Skill bytes are idempotent and
// changed bytes append one version. Any missing cause must fail the scenario,
// never be interpreted as an empty store or successful no-op.
//
// | Rule | C1 | C2 | C3 | C4 | C5 | Required effects |
// | F1 | yes | yes | yes | yes | yes | E1+E2+E3+E4+E5 |
// | F2 | missing/invalid | any | any | any | any | fail closed before invocation |
// | F3 | yes | yes | provider/turn fails | any | any | no fabricated terminal success |
// | F4 | yes | yes | yes | denied | any | no gated Resource mutation |
// | F5 | yes | yes | yes | yes | fails | no false publication success; retryable intent remains |
// Negative partitions F2-F5 are exercised at their authoritative unit/integration
// boundaries; F1 is the non-redundant real-process composition proof here.
//
// Run: (from e2e/)  node managed_full_chain_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
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
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const SKILL_HEADERS = { 'anthropic-beta': 'skills-2025-10-02' };
const SKILL_BETAS = ['skills-2025-10-02'];
const TMP = path.join(os.tmpdir(), `awaken-fullchain-e2e-${process.pid}`);
const STORE_DIR = `${TMP}/storage`;
const README = 'SEED_README_FULLCHAIN';
const REPO_MARKER = 'REPO_FULLCHAIN_8830'; // must match the fullChain behavior
const MEMO_MARKER = 'MEMO_FULLCHAIN_5521';
const ARTIFACT_MARKER = 'ARTIFACT_FULLCHAIN_9142';

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
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed'], work);
  const bare = `${TMP}/remote.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
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

// Files GET is a read-only Artifact projection; reverse resource operations belong
// to replacement/release.
async function listArtifacts(c, sid) {
  try {
    return await c.get(`/v1/files?scope_id=${sid}`);
  } catch {
    return null;
  }
}

async function main() {
  cleanupFixtureTree(TMP);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const bare = seedRemote();
  const upstream = await startUpstream('fullChain');
  const { server } = spawnServer('full-chain', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
    ...realServerEnv('fullChain', upstream, { mode: 'full-chain' }),
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
    let pushed = '';
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

    assert.equal(
      git(['rev-parse', 'main'], bare).trim(),
      remoteHeadBefore,
      'Files GET does not advance the Repository remote',
    );
    await c.beta.sessions.delete(session.id, { betas: BETAS });
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
      try {
        pushed = git(['show', 'main:CHAIN.txt'], bare).trim();
      } catch {
        pushed = '';
      }
      if (
        memContent.includes(MEMO_MARKER)
        && pushed === REPO_MARKER
        && listedArray.some((file) =>
          (file.filename ?? file.path ?? file.logical_path ?? '').includes('result.txt'))
      ) break;
      await sleep(200);
    }

    // 4) Skill was discovered + used.
    const toolNames = evs.filter((e) => e.type === 'agent.tool_use').map((e) => e.name);
    assert.ok(toolNames.includes('list_skills'), `skill discovery ran: ${JSON.stringify(toolNames)}`);
    assert.ok(toolNames.includes('Skill'), 'the greet skill was activated via the Skill tool');
    pass('skill discovered + activated (list_skills → Skill) in the same conversation');

    // 5) Memory store write-back landed.
    assert.ok(memContent.includes(MEMO_MARKER), `memory-store write-back landed: ${JSON.stringify(memContent)}`);
    pass('Session release reconciled memory_store write under its id');

    // 6) Repo commit + push-back to the real bare remote.
    assert.equal(pushed, REPO_MARKER, 'the agent edit was committed + pushed to the remote');
    pass('Session release published the Agent-authored Repository commit');

    // 7) Output artifact retrievable via the Files API.
    const artifacts = files?.data ?? files?.files ?? files ?? [];
    const arr = Array.isArray(artifacts) ? artifacts : artifacts.data ?? [];
    const artifact = arr.find((f) => (f.filename ?? f.path ?? f.logical_path ?? '').includes('result.txt'));
    assert.ok(artifact, `the output artifact is listed by /v1/files: ${JSON.stringify(arr)}`);
    const artifactResponse = await fetch(`http://127.0.0.1:${PORT}/v1/files/${artifact.id}/content`, {
      headers: {
        'x-api-key': 'e2e-dummy',
        'anthropic-beta': BETAS.join(','),
      },
    });
    const artifactBytes = await artifactResponse.text();
    assert.equal(artifactResponse.status, 200, `artifact content is downloadable: ${artifactBytes}`);
    assert.equal(artifactBytes, ARTIFACT_MARKER, 'downloaded File bytes equal the sandbox output');
    pass('output artifact projected, listed, and downloaded with exact bytes via /v1/files');

    // 8) Cross-session memory: the extractor sub-run saved a memory the store persisted.
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
    await c.beta.sessions.delete(consumingSession.id, { betas: BETAS });
    pass('agent-authored Skill versions persist without implicitly mutating Agent selection');

    console.log('E2E PASS: full chain — config → mounts → skill → memory + repo write-back → artifact → authored Skill versions.');
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
