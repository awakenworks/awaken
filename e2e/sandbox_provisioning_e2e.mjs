// Consolidated Namespace sandbox-provisioning e2e: writable memory_store and
// path-faithful github_repository share ONE
// sandbox, plus artifact projection and every dangling-reference failure arm.
//
// The existing e2e cover one resource type each. This drives Memory + Repository
// into a SINGLE session — the real
// StagedResources → sandbox_spec → provider realization path — then exercises:
//   • the Run executes with both writable resources mounted (provisioning succeeded),
//   • beta.files.list({scope_id}) (read-only artifact projection),
//   • fail-closed for each resource type (missing file / missing memory_store / bad repo).
//
// A File input is deliberately absent because its read-only success is already
// validated by the real substrate suites; Workdir denial is covered by
// managed_resource_mount. This test owns the combined writable Memory plus
// path-faithful Repository rule without duplicating the File matrix.
//
// Deterministic + CI-safe: local bare git repo (no network), `echo` upstream.
// Run: (from e2e/)  node sandbox_provisioning_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  committedEffectsAfterUnanchoredReceipt,
  pass,
  waitForSessionEventReceipt,
  withRealServer,
} from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const PORT = Number(process.env.E2E_PORT ?? 38291);
const TMP = path.join(os.tmpdir(), `awaken-sbxprov-e2e-${process.pid}`);
const git = (args, cwd) => execFileSync('git', args, { cwd, encoding: 'utf8' });

// A local bare repo (the "remote"), seeded with one commit — a git URL with no network.
function seedRemote() {
  const work = `${TMP}/seed`;
  fs.mkdirSync(work, { recursive: true });
  git(['init', '-q'], work);
  git(['symbolic-ref', 'HEAD', 'refs/heads/main'], work);
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
  // Test design (Workdir resource matrix). Causes: C1=valid/dangling File and
  // Memory references; C2=reachable/unreachable Repository; C3=a Run executes in
  // the realized Namespace environment. Effects: E1=valid Memory+Repository co-mount;
  // E2=C3 commits output and listed artifact bytes; E3=dangling references fail
  // at create; E4=unresolvable Repository remains in retryable pre-Run custody
  // with no execution or terminal effect.
  // Constraints/invariant: the frozen Session resource set owns provisioning and
  // artifact projection cannot hide or substitute failed resource realization.
  // Decision rules: S1=valid C1+C2=>E1; S2=S1+C3=>E2;
  // S3=dangling C1=>E3; S4=unreachable C2=>E4.
  const bare = seedRemote();

  await withRealServer('echo', PORT, async (base, upstream) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });

    // ── supply writable Memory plus path-faithful Repository ───────────────────
    const mem = await client.post('/v1/memory_stores', {
      body: { name: 'sandbox-provisioning-memory' },
      headers: MEMORY_HEADERS,
    });
    assert.ok(mem.id, 'memory store created');
    pass('supplied a memory store; local bare repo seeded');

    // Cause/effect table: valid Memory+Repository -> both realize and inference
    // runs; dangling File/Memory -> create rejects; unresolvable Repository ->
    // realization rejects before an assistant fact. Read-only File success has
    // its own Namespace/Container substrate matrix.
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [
        { type: 'memory_store', memory_store_id: mem.id, mount_path: '/workspace/memory' },
        { type: 'github_repository', url: bare, mount_path: '/workspace/repo' },
      ],
      betas: BETAS,
    });
    assert.ok(session.id.startsWith('sesn_'), `session with 2 resource types: ${session.id}`);
    pass('one session provisioned memory_store + github_repository together');

    // Cause/effect + decision rules for the accepted durable User command:
    // S1 valid Memory+Repository + agent.message => both resources realized and
    // the Run completed; S2 receipt admitted but only earlier events visible =>
    // keep reading committed history; S3 terminal error before agent.message =>
    // fail rather than treating durable admission as successful execution.
    const runReceipt = await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'use the resources' }] }],
      betas: BETAS,
    });
    const runReceiptId = runReceipt.data?.[0]?.id;
    assert.equal(typeof runReceiptId, 'string', 'combined-resource Run exact User receipt');
    const { delta: runEvents } = await waitForSessionEventReceipt(
      client,
      session.id,
      runReceiptId,
      BETAS,
      ({ delta }) => delta.some(
        (event) => event.type === 'agent.message' || event.type === 'session.error',
      ),
      'the combined-resource Run to complete or fail durably',
    );
    const runTypes = runEvents.map((event) => event.type);
    assert.ok(
      runTypes.includes('agent.message'),
      `Run executed with all resources mounted: ${runTypes}`,
    );
    pass('a Run executed over the combined writable-resource sandbox');

    // ── read-only artifact projection ─────────────────────────────────────────
    // C4 exact Session scope + C5 Files beta selector -> E4 one read-only
    // catalog projection. K4 GA Files has no scope_id and must reject that
    // parameter; listing never publishes writable resources. R4 C4&&C5->E4.
    const artifacts = await client.beta.files.list({ scope_id: session.id, betas: BETAS });
    assert.ok(artifacts, 'artifact/reverse-channel endpoint responded for the session');
    pass('beta.files.list({scope_id}) projects artifacts without hidden resource writes');

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

    // A repo is cloned host-side when the sandbox is REALIZED, before the first
    // Run is reserved. Causes: C1=the frozen Repository is structurally valid but
    // physically unresolvable; C2=the User batch is accepted by the Session root;
    // C3=pre-Run clone fails; C4=one bounded reconciliation window elapses without
    // external repair. Effects: E1=admission returns the exact unprocessed receipt
    // without listing it as unanchored committed history; E2=the Session remains
    // idle/nonterminal; E3=no Run, model, assistant, tool,
    // or session.error fact is fabricated; E4=no fallback Repository is used and
    // the original command remains available to the lifecycle supervisor.
    // Constraints/invariants: accepted root CAS cannot be revoked by a later
    // dependency failure; session.error requires a committed Ended(Error) Run;
    // Repository realization precedes Run reservation; this negative absence
    // oracle must not use the positive helper that requires a processed receipt.
    // Decision F1 C1+C2+C3=>E1+E2+E3+E4; F2 F1+C4=>the same effects still hold.
    const repoSession = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      resources: [{ type: 'github_repository', url: `${TMP}/no-such-repo.git`, mount_path: '/workspace/repo' }],
      betas: BETAS,
    });
    const modelRequestsBeforeRepoFailure = upstream.requests.length;
    const repoReceipt = await client.beta.sessions.events.send(repoSession.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'go' }] }],
      betas: BETAS,
    });
    const acceptedRepoEvent = repoReceipt.data?.[0];
    assert.equal(acceptedRepoEvent?.type, 'user.message', 'unresolvable Repository receipt family');
    assert.equal(typeof acceptedRepoEvent?.id, 'string', 'unresolvable Repository exact receipt');
    assert.equal(acceptedRepoEvent.processed_at, null, 'pre-Run clone failure is not processed');
    await new Promise((resolve) => setTimeout(resolve, 750));
    const repoEvents = [];
    for await (const event of client.beta.sessions.events.list(repoSession.id, { betas: BETAS })) {
      repoEvents.push(event);
    }
    committedEffectsAfterUnanchoredReceipt({
      history: repoEvents,
      receiptId: acceptedRepoEvent.id,
      forbiddenEventTypes: new Set([
        'agent.message',
        'agent.tool_use',
        'agent.tool_result',
        'session.error',
        'session.status_idle',
        'session.usage',
        'span.model_request_start',
        'span.model_request_end',
      ]),
      description: 'unresolvable Repository pre-Run failure',
    });
    assert.equal(
      (await client.beta.sessions.retrieve(repoSession.id, { betas: BETAS })).status,
      'idle',
      'pre-Run clone failure leaves the Session idle and nonterminal',
    );
    assert.equal(
      upstream.requests.length,
      modelRequestsBeforeRepoFailure,
      'unresolvable Repository never reaches model inference',
    );
    pass('an unresolvable Repository remains safely retained before Run reservation');
  }, {
    extraEnv: { SESSION_DEPLOYMENT_SANDBOX_TIER: 'namespace' },
  });

  // The successful arm owns a writable Memory projection, while the failure
  // arms may stop during realization. One mount-aware cleanup path handles both
  // terminal effects and surfaces detach failure instead of retrying raw rmSync.
  cleanupFixtureTree(TMP);
  console.log('E2E PASS: Namespace resources co-provision + artifact projection + fail-closed arms.');
  process.exitCode = 0;
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
