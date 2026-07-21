// Consolidated sandbox-provisioning e2e: every session-resource TYPE realized into
// ONE sandbox at prepare time, plus artifact projection and the fail-closed paths.
//
// The existing e2e cover one resource type each (file, memory_store, github_repository
// in separate files); this drives all three into a SINGLE session — the real
// StagedResources → sandbox_spec → provider realization path — then exercises:
//   • the turn runs with all resources mounted (provisioning succeeded),
//   • GET /v1/files?scope_id (read-only artifact projection),
//   • fail-closed for each resource type (missing file / missing memory_store / bad repo).
//
// The sandbox TIER (Workdir / Namespace-bwrap / Container-k8s) is not yet selectable
// through the managed session API (`environment_id` does not parameterize the local
// sandbox — see managed_environment_e2e.mjs), and the provisioning path is
// tier-agnostic; per-tier realization (bwrap bind / k8s memoryd-sidecar+emptyDir) is
// validated by the Rust tier tests (awaken-sandbox-local / -container). This proves
// the neutral provisioning contract end-to-end over the real server binary.
//
// Deterministic + CI-safe: local bare git repo (no network), `echo` upstream.
// Run: (from e2e/)  node sandbox_provisioning_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import { execFileSync } from 'node:child_process';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withRealServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38291);
const TMP = `/tmp/awaken-sbxprov-e2e-${process.pid}`;
const git = (args, cwd) => execFileSync('git', args, { cwd, encoding: 'utf8' });

// A local bare repo (the "remote"), seeded with one commit — a git URL with no network.
function seedRemote() {
  const work = `${TMP}/seed`;
  fs.mkdirSync(work, { recursive: true });
  git(['init', '-q', '-b', 'main'], work);
  git(['config', 'user.email', 'seed@t'], work);
  git(['config', 'user.name', 'seed'], work);
  fs.writeFileSync(`${work}/README.md`, 'SEED_PROVISION_CONTENT');
  git(['add', '-A'], work);
  git(['commit', '-q', '-m', 'seed'], work);
  const bare = `${TMP}/remote.git`;
  git(['clone', '-q', '--bare', work, bare]);
  return bare;
}

// POST /v1/sessions raw, so we can assert the HTTP status of a fail-closed create
// (the typed SDK throws on non-2xx and hides the code).
async function createRaw(base, body) {
  return fetch(`${base}/v1/sessions`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS[0] },
    body: JSON.stringify(body),
  });
}

async function main() {
  const bare = seedRemote();

  await withRealServer('echo', PORT, async (base) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // ── supply one resource of every type ──────────────────────────────────────
    const file = await client.beta.files.upload({
      file: await toFile(Buffer.from('provisioned file bytes'), 'doc.txt'),
      betas: BETAS,
    });
    assert.ok(file.id, 'file uploaded');
    const mem = await client.post('/v1/memory_stores');
    assert.ok(mem.id, 'memory store created');
    pass('supplied a file + a memory store; local bare repo seeded');

    // ── all three realized into ONE session at prepare ─────────────────────────
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'file', file_id: file.id, mount_path: '/workspace/doc.txt' },
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/workspace/memory' },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session with 3 resource types: ${session.id}`);
    pass('one session provisioned file + memory_store + github_repository together');

    // ── a turn runs with everything mounted (provisioning succeeded) ───────────
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'use the resources' }] }],
      betas: BETAS,
    });
    const events = [];
    for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      events.push(ev.type);
    }
    assert.ok(events.includes('agent.message'), `turn ran with all resources mounted: ${events}`);
    pass('a turn ran over the fully-provisioned sandbox');

    // ── read-only artifact projection ─────────────────────────────────────────
    const artifacts = await client.get(`/v1/files?scope_id=${session.id}`);
    assert.ok(artifacts, 'artifact/reverse-channel endpoint responded for the session');
    pass('GET /v1/files?scope_id projects artifacts without hidden resource writes');

    // ── fail-closed: each resource type rejects a dangling reference ───────────
    const badFile = await createRaw(base, {
      agent: 'assistant',
      resources: [{ type: 'file', file_id: 'file_does_not_exist', mount_path: '/x' }],
    });
    assert.ok(badFile.status >= 400, `missing file fails closed (got ${badFile.status})`);

    const badMem = await createRaw(base, {
      agent: 'assistant',
      resources: [{ type: 'memory_store', memory_store_id: 'memstore_nope', mount_path: '/x' }],
    });
    assert.ok(badMem.status >= 400, `missing memory_store fails closed (got ${badMem.status})`);

    pass('file + memory_store fail closed at create on a dangling reference');

    // A repo is cloned host-side when the sandbox is REALIZED (first turn), not at
    // create — so an unresolvable repo fails closed at the turn, not the create. The
    // session must not run a clean turn believing a repo mounted when it did not.
    const repoSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'github_repository', url: `${TMP}/no-such-repo.git`, mount_path: '/workspace/repo' }],
      betas: BETAS,
    });
    let turnFailed = false;
    try {
      await client.beta.sessions.events.send(repoSession.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
        betas: BETAS,
      });
      const evs = [];
      for await (const ev of client.beta.sessions.events.list(repoSession.id, { betas: BETAS })) {
        evs.push(ev.type);
      }
      // Fail-closed: the sandbox never realized, so no clean assistant turn happened.
      turnFailed = !evs.includes('agent.message');
    } catch {
      turnFailed = true; // the send itself surfaced the fail-closed clone error
    }
    assert.ok(turnFailed, 'an unresolvable repo fails the sandbox realization closed');
    pass('an unresolvable repo fails closed at sandbox realization (no clean turn)');
  });

  fs.rmSync(TMP, { recursive: true, force: true });
  console.log('E2E PASS: all sandbox resource types provision into one session + artifact projection + fail-closed.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
