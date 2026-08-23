// Cause-effect E2E for ADR-0063 durable resource reclamation through the real
// `awaken` composition root. It covers process death after logical delete,
// shared-File retention, Memory head+history purge, Skill tombstone purge, and
// durable receipts. IAM is intentionally not queried by the background worker.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import {
  managedFileUploadForm,
  managedWorkspaceClient,
  spawnProduction,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import { sqliteExec, sqliteRows, sqliteScalar } from './sqlite.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38435);
const WS_A = `reclaim-a-${process.pid}`;
const WS_B = `reclaim-b-${process.pid}`;
const AGENT = 'resource-reclamation-agent';
const MODEL = 'resource-reclamation-model';
const FAKE_KEY = 'sk-resource-reclamation-fake'; // awaken-allow: secret
const MANAGED_BETA = 'managed-agents-2026-04-01';
const MEMORY_BETA = 'agent-memory-2026-07-22';
const SKILLS_BETA = 'skills-2025-10-02';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory) {
  return spawnProduction(directory, PORT, {
    workspace: WS_A,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    // Sandbox capability decision table: S1 Workdir + writable projection =>
    // the portable provider is sufficient; S2 Workdir + enforced read-only
    // projection => the Workdir provider must fail placement; S3 the same S2
    // requirement + the existing Namespace provider => one capable Worker
    // realizes the generation. This scenario binds a read-only File, so it owns
    // S3 explicitly instead of weakening the production placement requirement.
    fields: {
      sandbox_tier: 'namespace',
      sandbox_dir: path.join(directory, 'sandboxes'),
    },
  });
}

async function ready() {
  await waitForPort(PORT, 60_000);
}

async function stop(child, signal = 'SIGINT') {
  if (signal === 'SIGINT') return stopServer(child);
  if (child.exitCode !== null) return;
  child.kill(signal);
  await new Promise((resolve) => child.once('exit', resolve));
}

const scoped = (workspace, suffix) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${workspace}/${suffix}`;

async function json(method, url, body) {
  // Protocol cause/effect rules: H1 Session/Memory/Skill route selects exactly
  // Managed/Memory/Skill beta and reaches reclamation; H2 a missing beta or H3
  // Managed+replacement Memory together yields an outer 400 and covers no
  // lifecycle rule; H4 File/resource-plane routes carry no unrelated beta.
  const beta = url.includes('/memory_stores')
    ? MEMORY_BETA
    : url.includes('/skills')
      ? SKILLS_BETA
      : url.includes('/sessions')
        ? MANAGED_BETA
        : undefined;
  const response = await fetch(url, {
    method,
    headers: {
      ...(beta === undefined ? {} : { 'anthropic-beta': beta }),
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let parsed = null;
  if (text) {
    try { parsed = JSON.parse(text); } catch { parsed = text; }
  }
  return { status: response.status, body: parsed };
}

async function upload(workspace, content, filename = 'input.txt') {
  const response = await fetch(scoped(workspace, 'files'), {
    method: 'POST',
    body: managedFileUploadForm(content, filename),
  });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

async function uploadSkill(workspace, content) {
  const form = new FormData();
  form.append('display_title', 'Reclamation Skill');
  form.append('files[]', new Blob([content], { type: 'text/markdown' }), 'SKILL.md');
  const response = await fetch(scoped(workspace, 'skills'), {
    method: 'POST',
    headers: { 'anthropic-beta': SKILLS_BETA },
    body: form,
  });
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  return { status: response.status, body };
}

async function authorModel(upstream) {
  const provider = await json('POST', scoped(WS_A, 'config/provider-connections'), {
    idempotency_key: 'resource-reclamation-provider',
    workspace_id: WS_A,
    provider_id: 'anthropic',
    display_name: 'Anthropic',
    dialect: 'anthropic_messages',
    base_url: `${upstream.url}/v1/`,
    timeout_secs: 30,
    secret: FAKE_KEY,
  });
  assert.equal(provider.status, 201, JSON.stringify(provider.body));
  const agent = await json('PUT', scoped(WS_A, `config/agents/${AGENT}`), {
    name: AGENT, model: { id: MODEL }, system: 'resource reclamation', max_steps: 2,
  });
  assert.equal(agent.status, 200, JSON.stringify(agent.body));
  const publication = await json('POST', scoped(WS_A, `config/agents/${AGENT}/publish`));
  assert.equal(publication.status, 200, JSON.stringify(publication.body));
}

async function driveSession(client, sessionId) {
  // Run/realization decision table: C1=official SDK send returns one exact User
  // receipt; C2=the owning Worker processes C1; C3=a later Agent reply and idle
  // edge commit. E1=C1+C2+C3 permits DB/purge-effect assertions. Constraint K:
  // neither HTTP admission nor a pre-existing idle state can acknowledge the
  // Resource generation. Rules D1 !C1=>fail; D2 C1+(!C2||!C3)=>retry for the
  // predecessor lease bound; D3 C1+C2+C3=>E1.
  const response = await client.beta.sessions.events.send(sessionId, {
    events: [{
      type: 'user.message',
      content: [{ type: 'text', text: 'realize the reclamation Resource generation' }],
    }],
    betas: [MANAGED_BETA],
  });
  assert.equal(response.data?.length, 1, JSON.stringify(response));
  const receiptId = response.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'D1 exact reclamation User Event receipt');
  return waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    [MANAGED_BETA],
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    'the reclamation Resource Run to commit its reply and idle edge',
    { timeoutMs: 90_000 },
  );
}

function receipts(directory) {
  const database = path.join(directory, 'resources.db');
  if (!fs.existsSync(database)) return [];
  return sqliteRows(database, 'SELECT data FROM resource_lifecycle_purge_intents')
    .map((row) => JSON.parse(row.data));
}

async function waitReceipt(
  directory,
  kind,
  resourceId,
  { workspace, timeoutMs = 45_000 } = {},
) {
  // Receipt identity decision table: R1 File logical id => match the canonical
  // delete idempotency key because the physical target is its content-addressed
  // blob; R2 every other resource id => match the target directly. Conflating
  // the two File identities would make deduplicated/shared blobs untraceable.
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const receipt = receipts(directory).find(
      (intent) => intent.target.kind === kind
        && (kind === 'file'
          ? intent.idempotency_key === `file-delete:${workspace}:${resourceId}`
          : intent.target.resource_id === resourceId)
        && intent.status === 'completed',
    );
    if (receipt) return receipt;
    await sleep(200);
  }
  throw new Error(
    `no completed ${kind}/${resourceId} purge receipt: ${JSON.stringify(receipts(directory))}`,
  );
}

function scalar(database, sql) {
  return Number(sqliteScalar(database, sql));
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function blobForFile(directory, workspace, fileId) {
  // File id is the workspace-scoped logical handle; blob id is the canonical
  // physical row. Keep that mapping in one helper so assertions never query the
  // blob table with a logical id and pass accidentally on a missing row.
  return String(sqliteScalar(
    path.join(directory, 'files.db'),
    `SELECT blob_id FROM file_store_file
       WHERE id=${sqlQuote(fileId)} AND workspace_id=${sqlQuote(workspace)}`,
  ));
}

async function waitForLifecycleSchema(directory, timeoutMs = 20_000) {
  const database = path.join(directory, 'resources.db');
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const version = scalar(
        database,
        `SELECT count(*) FROM resource_lifecycle_schema_migrations
           WHERE bundle_id='awaken.resource_lifecycle' AND version=1`,
      );
      if (version === 1) return version;
    } catch {
      // TCP readiness can precede optional resource-plane migration visibility.
      // A read-only open intentionally does not create a misleading empty DB.
    }
    await sleep(50);
  }
  throw new Error('resource lifecycle migration did not become visible');
}

function seedRepository(root) {
  const work = path.join(root, 'reclamation-repository-work');
  const remote = path.join(root, 'reclamation-repository.git');
  fs.mkdirSync(work, { recursive: true });
  execFileSync('git', ['init', '-q'], { cwd: work });
  execFileSync('git', ['symbolic-ref', 'HEAD', 'refs/heads/main'], { cwd: work });
  execFileSync('git', ['config', 'user.email', 'resource-reclaim@example.invalid'], { cwd: work });
  execFileSync('git', ['config', 'user.name', 'resource-reclaim'], { cwd: work });
  fs.writeFileSync(path.join(work, 'README.md'), 'reclamation repository');
  execFileSync('git', ['add', 'README.md'], { cwd: work });
  execFileSync('git', ['commit', '-q', '-m', 'seed'], { cwd: work });
  execFileSync('git', ['clone', '-q', '--bare', work, remote]);
  return remote;
}

async function main() {
  // Test design (reclamation lifecycle). Causes: C1=logical deletion/revocation;
  // C2=active Session/Workspace references remain or are gone; C3=the process
  // crashes before physical receipt; C4=a registered Worker owns activation and
  // terminal cleanup. Effects: E1=C1 denies new access immediately; E2=C2/live
  // retains shared bytes; E3=C2/gone permits one evidenced purge; E4=C3 resumes
  // the same intent; E5=C4 gates cleanup on exact Runtime receipts.
  // Constraints/invariant: one durable lifecycle intent plus intrinsic reference
  // count owns physical deletion; API success alone never claims reclamation.
  // Decision rules: R1=C1+C2(live)=>E1+E2; R2=C1+C2(gone)=>E1+E3;
  // R3=R1/R2+C3=>E4; R4=R2+C4=>E5 then E3.
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-reclaim-'));
  const upstream = await startFakeAnthropic(FAKE_KEY, { models: [MODEL] });
  let server = start(directory);
  try {
    await ready();
    await authorModel(upstream);
    const client = managedWorkspaceClient(`http://127.0.0.1:${PORT}`, WS_A);
    assert.equal(
      // Readiness cause graph: C1 TCP listener ready; C2 resource-plane DB exists;
      // C3 scoped migration is committed. Only C1+C2+C3 means the API composition
      // is fully observable; TCP alone must not let the fixture create an empty DB.
      await waitForLifecycleSchema(directory),
      1,
      'resource lifecycle schema is applied through its scoped migration ledger',
    );

    // Crash after the API committed durable intent + logical revoke. Startup
    // recovery must finish the same intent without another delete request.
    // Authentication decision table: R1 generic production fixture + no-login
    // identity => the scoped resource API is directly available; R2 an
    // explicitly self-managed fixture => bearer/session authentication is
    // required (covered by the management-auth E2Es). R1 must be configured in
    // the fixture's own config.toml, not inherited from another HOME.
    // The temporary intrinsic hold closes the scheduler race deterministically:
    // C1 durable delete + hold => intent remains pending before SIGKILL; C2 hold
    // removed while the process is dead => no second API command exists; C3
    // replacement generation becomes authoritative => startup recovery alone
    // completes the original intent.
    const crashedFile = await upload(WS_A, 'crash-recovery');
    const crashedBlob = blobForFile(directory, WS_A, crashedFile);
    sqliteExec(
      path.join(directory, 'resources.db'),
      `INSERT INTO resource_lifecycle_references(
         workspace_id, resource_kind, resource_id, reference_kind, reference_id
       ) VALUES (
         ${sqlQuote(WS_A)}, 'file', ${sqlQuote(crashedBlob)},
         'retention_hold', 'crash-recovery-hold'
       )`,
    );
    assert.equal((await json('DELETE', scoped(WS_A, `files/${crashedFile}`))).status, 200);
    await stop(server, 'SIGKILL');
    sqliteExec(
      path.join(directory, 'resources.db'),
      `DELETE FROM resource_lifecycle_references
         WHERE workspace_id=${sqlQuote(WS_A)}
           AND resource_kind='file'
           AND resource_id=${sqlQuote(crashedBlob)}
           AND reference_kind='retention_hold'
           AND reference_id='crash-recovery-hold'`,
    );
    server = start(directory);
    await ready();
    // Crash-recovery decision table: R1 graceful stop -> old generation is Dead
    // and replacement registers immediately; R2 SIGKILL + unexpired lease ->
    // replacement waits without stealing authority; R3 lease expiry -> the same
    // registration path advances the generation and resumes durable reclamation.
    const crashReceipt = await waitReceipt(directory, 'file', crashedFile, { workspace: WS_A });
    assert.equal(crashReceipt.receipt.evidence.blob_deleted, true);
    assert.equal(
      scalar(path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id=${sqlQuote(crashedBlob)}`),
      0,
    );

    // Authorization has already allowed the logical File deletion, but a live
    // Session binding is intrinsic resource state and independently blocks physical
    // reclamation. The reclaimer consumes only that reference edge; after Session
    // archive removes it, the same durable intent converges without an IAM query.
    const boundFile = await upload(WS_A, 'session-bound-content');
    const boundBlob = blobForFile(directory, WS_A, boundFile);
    const repository = seedRepository(directory);
    const boundSession = await client.beta.sessions.create({
      agent: AGENT, environment_id: 'env_local',
      resources: [{
        type: 'github_repository',
        url: repository,
        mount_path: '/workspace/reclamation-repository',
      }],
      betas: [MANAGED_BETA],
    });
    let binding = await json(
      'POST',
      scoped(WS_A, `sessions/${boundSession.id}/resources`),
      { type: 'file', file_id: boundFile, mount_path: '/workspace/bound.txt' },
    );
    // Worker-replacement/resource-activation cause graph: C1 the replacement
    // Worker owns its lease; C2 an initial generation is pending; C3 an external
    // attempt/lease has observed that generation. A File mutation with !C2 starts
    // the normal phase; C2+!C3 amends that one unattempted generation without a
    // second revision; C2+C3 rejects until recovery settles it, then the exact
    // retry commits once. This distinguishes durable intent from external work.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // | R1   | T  | F  | n/a | accept prepare; observe idle after activation |
    // | R2   | any | T  | F | amend pending generation immediately |
    // | R3   | F  | T  | T | reject; retain the observed generation |
    // | R4   | T  | F  | n/a | retry commits once; observe idle after activation |
    if (binding.status === 400) {
      assert.match(
        String(binding.body?.error?.message ?? ''),
        /resource activation is already pending/,
        'R2 fails closed only for the durable pending generation',
      );
      // This observer only gates the exact retry of a resource mutation that
      // raced an already-observed generation; the later Run completion is owned
      // solely by the receipt-scoped SDK observer in driveSession.
      await waitForValue(
        () => client.beta.sessions.retrieve(boundSession.id, { betas: [MANAGED_BETA] }),
        (session) => session.status === 'idle',
        'the observed pending generation to settle before its exact mutation retry',
        { timeoutMs: 90_000, pollMs: 200 },
      );
      binding = await json(
        'POST',
        scoped(WS_A, `sessions/${boundSession.id}/resources`),
        { type: 'file', file_id: boundFile, mount_path: '/workspace/bound.txt' },
      );
    }
    assert.equal(binding.status, 200, JSON.stringify(binding.body));
    // Registered-Worker realization cause graph: C1 the replacement Worker owns
    // its registry lease; C2 the Repository+File generation is durably pending;
    // C3 the Agent has one exact executable publication; C4 a User Event creates
    // the subordinate Run claim. Effects: E1 C1+C2+C3 without C4 truthfully stays
    // Rescheduling; E2 C1+C2+C4 without C3 fails before dispatch; E3 all causes
    // make that one Worker realize the exact generation and settle Idle. Only E3
    // permits archive, so the test must not treat listener/receipt readiness as
    // a physical activation acknowledgement.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | D1 | T | T | T | F | retain Rescheduling; no physical claim |
    // | D2 | T | T | F | T | fail closed before Worker dispatch |
    // | D3 | T | T | T | T | exact generation Active; Session Idle |
    await driveSession(client, boundSession.id);
    // The exact processed User receipt plus its later reply/idle is the sole Run
    // completion oracle. From here onward only independent DB and purge effects
    // may claim physical activation or reclamation.
    assert.equal((await json('DELETE', scoped(WS_A, `files/${boundFile}`))).status, 200);
    await sleep(5_500);
    assert.ok(
      !receipts(directory).some(
        (intent) => intent.target.resource_id === boundBlob && intent.status === 'completed',
      ),
      'live Session binding defers the physical purge',
    );
    assert.equal(
      scalar(
        path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id=${sqlQuote(boundBlob)}`,
      ),
      1,
      'logical denial does not remove bytes while an intrinsic reference remains',
    );
    // Registered-Worker terminal-cleanup cause/effect graph: C1 an Idle remote
    // Session has no cleanup command; C2 archive durably freezes the command;
    // C3 its owning Worker has not or has recorded the exact Runtime receipt.
    // Effects: E1 C1+C2+!C3 returns the typed 503 without claiming completion;
    // E2 C1+C2+C3 makes the same idempotent archive return 200 and only then
    // permits resource reclamation. A local inline Runtime's direct 200 is a
    // different placement rule and must not replace this remote protocol.
    //
    // | Rule | command | exact receipt | Effect |
    // |---|---|---|---|
    // | T1 | newly frozen | no | exact cleanup-pending 503 |
    // | T2 | same command | yes | archive 200; terminal cleanup complete |
    // Constraints/invariant: only the owning Worker's exact idempotent Runtime
    // receipt closes terminal cleanup and unlocks reclamation.
    let sawCleanupPending = false;
    const archive = await waitForValue(
      async () => {
        const response = await json(
          'POST',
          scoped(WS_A, `sessions/${boundSession.id}/archive`),
        );
        if (response.status === 503) {
          assert.match(
            String(response.body?.error?.message ?? ''),
            /remote Session terminal cleanup remains pending: Session terminal cleanup has no Runtime receipt/,
            'T1 exposes only the canonical missing-receipt state',
          );
          sawCleanupPending = true;
        } else {
          assert.equal(response.status, 200, JSON.stringify(response.body));
        }
        return response;
      },
      (response) => response.status === 200,
      'the owning Worker did not settle the exact terminal cleanup receipt',
      { timeoutMs: 45_000, pollMs: 200 },
    );
    assert.equal(sawCleanupPending, true, 'T1 precedes the completed replay');
    assert.equal(archive.body.status, 'terminated', JSON.stringify(archive.body));
    const boundReceipt = await waitReceipt(directory, 'file', boundFile, { workspace: WS_A });
    assert.equal(boundReceipt.receipt.evidence.blob_deleted, true);
    const repositoryId = `managed:${boundSession.id}:repository:0`;
    const repositoryReceipt = await waitReceipt(directory, 'repository', repositoryId);
    assert.equal(repositoryReceipt.receipt.evidence.local_realizations_deleted, 0);

    // Equal bytes share one blob. Removing A cannot delete bytes still owned
    // to B; revoking B subsequently permits physical GC.
    const sharedA = await upload(WS_A, 'shared-content');
    const sharedB = await upload(WS_B, 'shared-content');
    const sharedBlob = blobForFile(directory, WS_A, sharedA);
    assert.notEqual(sharedA, sharedB, 'workspace ownership uses distinct logical File ids');
    assert.equal(
      sharedBlob,
      blobForFile(directory, WS_B, sharedB),
      'equal bytes reuse the one content-addressed physical blob',
    );
    assert.equal((await json('DELETE', scoped(WS_A, `files/${sharedA}`))).status, 200);
    await sleep(5_500);
    assert.equal((await json('GET', scoped(WS_B, `files/${sharedB}`))).status, 200);
    assert.equal(
      scalar(
        path.join(directory, 'files.db'),
        `SELECT count(*) FROM file_store_blob WHERE id=${sqlQuote(sharedBlob)}`,
      ),
      1,
      'the remaining logical owner retains the shared physical blob',
    );
    assert.equal((await json('DELETE', scoped(WS_B, `files/${sharedB}`))).status, 200);
    await waitReceipt(directory, 'file', sharedB, { workspace: WS_B });

    // Memory purge removes live heads and the version log from the same canonical
    // repository only after the store tombstone is visible.
    const memoryStore = await json('POST', scoped(WS_A, 'memory_stores'), { name: 'reclaim-me' });
    assert.equal(memoryStore.status, 200);
    const memoryId = memoryStore.body.id;
    assert.equal((await json('POST', scoped(WS_A, `memory_stores/${memoryId}/memories`), {
      path: '/fact.md', content: 'remember',
    })).status, 200);
    assert.equal((await json('DELETE', scoped(WS_A, `memory_stores/${memoryId}`))).status, 200);
    const memoryReceipt = await waitReceipt(directory, 'memory_store', memoryId);
    assert.equal(memoryReceipt.receipt.evidence.heads_deleted, 1);
    assert.equal(memoryReceipt.receipt.evidence.versions_deleted, 1);

    // Skill delete hides new resolution immediately, then removes the retained
    // immutable bundle once no Session/Agent binding remains.
    const skill = await uploadSkill(
      WS_A,
      '---\nname: reclaim-skill\ndescription: test\n---\nUse safely.',
    );
    assert.equal(skill.status, 200, JSON.stringify(skill.body));
    const skillId = skill.body.id;
    assert.equal((await json('DELETE', scoped(WS_A, `skills/${skillId}`))).status, 200);
    assert.equal((await json('GET', scoped(WS_A, `skills/${skillId}`))).status, 404);
    const skillReceipt = await waitReceipt(directory, 'skill', skillId);
    assert.equal(skillReceipt.receipt.evidence.versions_deleted, 1);

    console.log('E2E PASS: durable resource deny, crash recovery, reference guards, and per-kind receipts.');
  } finally {
    await stop(server).catch(() => {});
    await upstream.close();
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
