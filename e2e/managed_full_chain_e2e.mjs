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
// Run: (from e2e/)  node managed_full_chain_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38221);
const BETAS = ['managed-agents-2026-04-01'];
const TMP = `/tmp/awaken-fullchain-e2e-${process.pid}`;
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

const listEvents = async (c, sid) => {
  const evs = [];
  for await (const ev of c.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(ev);
  return evs;
};

// Release every gated (`ask`) tool call not yet approved — each `write` awaits for a
// confirmation, so approving lets the run advance to its terminal message.
async function approveGated(c, sid, evs, approved) {
  for (const e of evs) {
    if (e.type === 'agent.tool_use' && e.evaluated_permission === 'ask' && !approved.has(e.id)) {
      approved.add(e.id);
      await c.beta.sessions.events.send(sid, {
        events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
        betas: BETAS,
      });
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
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const bare = seedRemote();
  const upstream = await startUpstream('fullChain');
  const { server } = spawnServer('full-chain', PORT, {
    AWAKEN_STORAGE_DIR: STORE_DIR,
    ...realServerEnv('fullChain', upstream, { mode: 'full-chain' }),
  });
  try {
    await waitForPort(PORT);
    const c = client();

    // 1) Configure resources via the API: a fresh memory store + the github repo.
    const mem = await c.post('/v1/memory_stores');
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
    assert.ok(skillIds.includes('greet'), `the skill is offered: ${JSON.stringify(session.agent.skills)}`);
    assert.equal(session.resources.length, 2, 'both resources bound to the session');
    const remoteHeadBefore = git(['rev-parse', 'main'], bare).trim();
    pass('one session binds memory_store + github_repository and is offered the skill');

    // 3) A single natural-language turn drives the whole chain.
    await c.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'do the full chain' }] }],
      betas: BETAS,
    });

    // Drive await→approve until the turn ends. Artifact GET remains read-only.
    const approved = new Set();
    let evs = [];
    let files = null;
    let memContent = '';
    for (let i = 0; i < 60; i += 1) {
      await sleep(400);
      evs = await listEvents(c, session.id);
      await approveGated(c, session.id, evs, approved);
      files = await listArtifacts(c, session.id);
      const done = evs.some((e) => e.type === 'agent.message' && (e.content ?? []).some((b) => (b.text ?? '').includes('done')));
      if (done) break;
    }

    assert.equal(
      git(['rev-parse', 'main'], bare).trim(),
      remoteHeadBefore,
      'Files GET does not advance the Repository remote',
    );
    await c.beta.sessions.delete(session.id, { betas: BETAS });
    for (let i = 0; i < 20; i += 1) {
      try {
        const page = await c.get(`/v1/memory_stores/${mem.id}/memories`);
        memContent = (page?.data ?? []).map((memory) => memory.content ?? '').join('\n');
      } catch {
        memContent = '';
      }
      if (memContent.includes(MEMO_MARKER)) break;
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
    const pushed = git(['show', 'main:CHAIN.txt'], bare).trim();
    assert.equal(pushed, REPO_MARKER, 'the agent edit was committed + pushed to the remote');
    pass('Session release published the Agent-authored Repository commit');

    // 7) Output artifact retrievable via the Files API.
    const artifacts = files?.data ?? files?.files ?? files ?? [];
    const arr = Array.isArray(artifacts) ? artifacts : artifacts.data ?? [];
    const artifact = arr.find((f) => (f.filename ?? f.path ?? f.logical_path ?? '').includes('result.txt'));
    assert.ok(artifact, `the output artifact is listed by /v1/files: ${JSON.stringify(arr)}`);
    pass('output artifact projected + retrievable via GET /v1/files');

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
        betas: BETAS,
      });
      await c.beta.sessions.events.send(authored.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
        betas: BETAS,
      });
      const approved = new Set();
      let authoredEvents = [];
      for (let attempt = 0; attempt < 30; attempt += 1) {
        await sleep(200);
        authoredEvents = await listEvents(c, authored.id);
        await approveGated(c, authored.id, authoredEvents, approved);
        if (authoredEvents.some((event) =>
          event.type === 'agent.message' &&
          (event.content ?? []).some((content) => (content.text ?? '').includes(`authored ${prompt}`)),
        )) break;
      }
      assert.ok(
        authoredEvents.some((event) =>
          event.type === 'agent.message' &&
          (event.content ?? []).some((content) => (content.text ?? '').includes(`authored ${prompt}`)),
        ),
      );
      await c.beta.sessions.delete(authored.id, { betas: BETAS });
    };
    const skillVersions = async () => {
      const response = await c.get('/v1/skills/authored/versions');
      return response.data ?? [];
    };
    await author('author-skill-v1');
    assert.deepEqual((await skillVersions()).map((version) => version.version), ['1']);
    await author('author-skill-v1');
    assert.deepEqual(
      (await skillVersions()).map((version) => version.version),
      ['1'],
      're-harvesting identical SKILL.md bytes is idempotent',
    );
    await author('author-skill-v2');
    assert.deepEqual((await skillVersions()).map((version) => version.version), ['1', '2']);
    const authoredLatest = await c.get('/v1/skills/authored/versions/latest');
    assert.equal(authoredLatest.version, '2');
    assert.match(
      await (await fetch(`http://127.0.0.1:${PORT}/v1/skills/authored/versions/2/content`)).text(),
      /AUTHORED_SKILL_V2/u,
    );
    const consumingSession = await c.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    assert.ok(
      (consumingSession.agent.skills ?? []).some((skill) => (skill.skill_id ?? skill) === 'authored'),
      'a later Session advertises the durable authored Skill',
    );
    await c.beta.sessions.delete(consumingSession.id, { betas: BETAS });
    pass('agent-authored Skill harvest is idempotent and appends immutable changed versions');

    console.log('E2E PASS: full chain — config → mounts → skill → memory + repo write-back → artifact → authored Skill versions.');
  } finally {
    await stopServer(server);
    upstream.close();
    fs.rmSync(TMP, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exit(1);
});
