// Recoverable-fault E2E for the authorization-agnostic ResourceReclaimer.
// Durable SQLite faults model process/race boundaries without adding test-only
// HTTP hooks to production services.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  managedFileUploadForm,
  spawnProduction,
  stopServer,
  waitForPort,
} from './harness.mjs';
import { sqliteExec, sqliteRows, sqliteScalar } from './sqlite.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38440);
const WORKSPACE = `reclamation-faults-${process.pid}`;
const MANAGED_BETA = 'managed-agents-2026-04-01';
const SKILLS_BETA = 'skills-2025-10-02';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function start(directory, { captureStderr = false } = {}) {
  const child = spawnProduction(directory, PORT, {
    workspace: WORKSPACE,
    controlSealKey: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    stderr: captureStderr ? 'pipe' : 'inherit',
  });
  child.stderrText = '';
  if (captureStderr) {
    child.stderr.on('data', (chunk) => {
      const text = chunk.toString();
      child.stderrText += text;
      process.stderr.write(text);
    });
  }
  return child;
}

async function ready(child) {
  await waitForPort(PORT, 60_000, child);
}

async function stop(child, signal = 'SIGINT') {
  if (signal === 'SIGINT') return stopServer(child);
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once('exit', resolve));
  child.kill(signal);
  await exited;
}

const scoped = (tail) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${WORKSPACE}/${tail}`;

async function json(method, tail, body) {
  // Protocol decision rules: H1 Session tail => Managed beta; H2 Skill tail =>
  // Skill beta; H3 File tail => no unrelated beta. Missing H1/H2 is an outer
  // protocol 400, not a ResourceReclaimer fault effect.
  const beta = tail.startsWith('sessions')
    ? MANAGED_BETA
    : tail.startsWith('skills')
      ? SKILLS_BETA
      : undefined;
  const response = await fetch(scoped(tail), {
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

async function upload(content, filename) {
  const response = await fetch(scoped('files'), {
    method: 'POST',
    body: managedFileUploadForm(content, filename),
  });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

async function uploadSkill(content) {
  const form = new FormData();
  form.append('display_title', 'Reclamation Fault Skill');
  form.append('files[]', new Blob([content], { type: 'text/markdown' }), 'SKILL.md');
  const response = await fetch(scoped('skills'), {
    method: 'POST',
    headers: { 'anthropic-beta': SKILLS_BETA },
    body: form,
  });
  const text = await response.text();
  let body = null;
  if (text) {
    try { body = JSON.parse(text); } catch { body = text; }
  }
  assert.equal(response.status, 200, `create Skill: ${text}`);
  assert.equal(typeof body?.id, 'string', `create Skill returned an id: ${text}`);
  return body.id;
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return sqliteExec(database, sql);
}

function intents(directory) {
  const database = path.join(directory, 'resources.db');
  return sqliteRows(
    database,
    'SELECT data FROM resource_lifecycle_purge_intents ORDER BY intent_id',
  ).map((row) => JSON.parse(row.data));
}

function intentFor(directory, resourceId) {
  return intents(directory).find((intent) =>
    intent.target.resource_id === resourceId
      || intent.idempotency_key === `file-delete:${WORKSPACE}:${resourceId}`);
}

function logicalResourceId(intent) {
  const filePrefix = `file-delete:${WORKSPACE}:`;
  return intent.target.kind === 'file' && intent.idempotency_key.startsWith(filePrefix)
    ? intent.idempotency_key.slice(filePrefix.length)
    : intent.target.resource_id;
}

function blobForFile(database, fileId) {
  const blobId = sqliteScalar(
    database,
    `SELECT blob_id FROM file_store_file
       WHERE workspace_id=${sqlQuote(WORKSPACE)} AND id=${sqlQuote(fileId)}`,
  );
  assert.ok(blobId, `missing physical blob identity for logical File ${fileId}`);
  return String(blobId);
}

async function waitFor(directory, resourceIds, predicate, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const found = resourceIds.map((id) => intentFor(directory, id));
    if (found.every((intent) => intent && predicate(intent))) return found;
    await sleep(200);
  }
  throw new Error(`resource intents did not converge: ${JSON.stringify(intents(directory))}`);
}

async function waitForStderr(child, pattern, timeoutMs = 8_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (pattern.test(child.stderrText)) return;
    if (child.exitCode !== null) throw new Error(`awaken exited with ${child.exitCode}`);
    await sleep(100);
  }
  throw new Error(`stderr did not match ${pattern}: ${child.stderrText}`);
}

function encoded(value) {
  return Buffer.from(value).toString('hex');
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-reclamation-faults-'));
  const lifecycle = path.join(directory, 'resources.db');
  const files = path.join(directory, 'files.db');
  let server = start(directory);
  try {
    await ready(server);
    const releaseFailure = await upload('release-failure', 'release.txt');
    const contended = await upload('contended-fence', 'contended.txt');
    const lateReference = await upload('late-reference', 'late.txt');
    const durableBlockers = await upload('durable-blockers', 'blockers.txt');
    const corruptReference = await upload('corrupt-reference', 'corrupt-reference.txt');
    const alreadyOwned = await upload('already-owned-fence', 'already-owned.txt');
    const fencedBinding = await upload('fenced-binding', 'fenced-binding.txt');
    const physicalFailure = await upload('physical-failure', 'physical-failure.txt');
    const combinedFailure = await upload('combined-failure', 'combined-failure.txt');
    const lateGuardError = await upload('late-guard-error', 'late-guard-error.txt');
    const saveConflict = await upload('save-conflict', 'save-conflict.txt');
    const saveFailure = await upload('save-failure', 'save-failure.txt');
    const filesUnderReclamation = [
      releaseFailure,
      contended,
      lateReference,
      durableBlockers,
      corruptReference,
      alreadyOwned,
      physicalFailure,
      combinedFailure,
      lateGuardError,
      saveConflict,
      saveFailure,
    ];
    // Fault-injection identity decision table: File API calls use logical ids;
    // lifecycle fences/references and blob triggers use the content-addressed
    // physical id. Resolve that mapping once so no fault silently targets the
    // wrong aggregate. Skill ids are already physical lifecycle identities.
    const fileBlobs = new Map(
      [...filesUnderReclamation, fencedBinding]
        .map((fileId) => [fileId, blobForFile(files, fileId)]),
    );
    const blob = (fileId) => fileBlobs.get(fileId);
    const skillName = `fault-skill-${process.pid}`;
    const skillId = await uploadSkill(
      `---\nname: ${skillName}\ndescription: fault recovery\n---\nRecover safely.`,
    );
    // Fault-campaign admission table: C1 logical delete is durable; C2 the live
    // reclaimer may race before fault installation; C3 a durable retention hold
    // covers every target. C1+C2+!C3 can silently complete and invalidate the
    // intended rule; C1+C2+C3 remains Pending. Remove the one common hold only
    // after the crash and fault installation so every rule reaches its named
    // fence/guard/storage/physical failure.
    sqlite(
      lifecycle,
      `INSERT INTO resource_lifecycle_references(
         workspace_id, resource_kind, resource_id, reference_kind, reference_id
       ) VALUES (
         ${sqlQuote(WORKSPACE)}, 'skill', ${sqlQuote(skillId)},
         'retention_hold', 'fault-injection-hold'
       );
       INSERT INTO resource_lifecycle_references(
         workspace_id, resource_kind, resource_id, reference_kind, reference_id
       ) VALUES ${filesUnderReclamation.map((fileId) => `(
         ${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(fileId))},
         'retention_hold', 'fault-injection-hold'
       )`).join(',')};`,
    );

    for (const fileId of filesUnderReclamation) {
      assert.equal((await json('DELETE', `files/${fileId}`)).status, 200);
    }
    assert.equal((await json('DELETE', `skills/${skillId}`)).status, 200);
    await stop(server, 'SIGKILL');

    const skillAggregate = path.join(
      directory,
      'skills',
      encoded(WORKSPACE),
      `${encoded(skillId)}.json`,
    );
    const tombstone = fs.readFileSync(skillAggregate);
    const alreadyOwnedIntent = intentFor(directory, alreadyOwned);
    assert.ok(alreadyOwnedIntent, 'the logical delete durably scheduled reclamation');
    const saveConflictIntent = intentFor(directory, saveConflict);
    const saveFailureIntent = intentFor(directory, saveFailure);
    assert.ok(saveConflictIntent && saveFailureIntent);

    // The three triggers/rows model distinct production races at durable seams:
    // a foreign reclaimer already owns one identity; a reference appears after
    // the first guard scan; and release fails after idempotent physical deletion.
    sqlite(
      lifecycle,
      `
        INSERT INTO resource_lifecycle_reclamation_fences(resource_kind, resource_id, intent_id)
          VALUES ('file', ${sqlQuote(blob(contended))}, 'external-reclaimer');
        DELETE FROM resource_lifecycle_references
          WHERE resource_kind = 'skill' AND resource_id = ${sqlQuote(skillId)}
            AND reference_kind = 'retention_hold'
            AND reference_id = 'fault-injection-hold';
        DELETE FROM resource_lifecycle_references
          WHERE resource_kind = 'file'
            AND resource_id IN (${filesUnderReclamation.map((fileId) => sqlQuote(blob(fileId))).join(',')})
            AND reference_kind = 'retention_hold'
            AND reference_id = 'fault-injection-hold';
        INSERT INTO resource_lifecycle_reclamation_fences(resource_kind, resource_id, intent_id)
          VALUES ('skill', ${sqlQuote(skillId)}, 'external-skill-reclaimer');
        INSERT INTO resource_lifecycle_reclamation_fences(resource_kind, resource_id, intent_id)
          VALUES ('file', ${sqlQuote(blob(alreadyOwned))}, ${sqlQuote(alreadyOwnedIntent.intent_id)});
        INSERT INTO resource_lifecycle_references(
          workspace_id, resource_kind, resource_id, reference_kind, reference_id
        ) VALUES
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'logical_lifecycle', 'logical-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'workspace_ownership', 'owner-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'agent_binding', 'agent-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'artifact', 'artifact-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'runtime_handle', 'runtime-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'extraction_intent', 'extract-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(durableBlockers))}, 'retention_hold', 'hold-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(corruptReference))}, 'future_unknown_kind', 'bad-1');
        CREATE TRIGGER inject_late_reference
          AFTER INSERT ON resource_lifecycle_reclamation_fences
          WHEN NEW.resource_id = ${sqlQuote(blob(lateReference))}
        BEGIN
          INSERT INTO resource_lifecycle_references(
            workspace_id, resource_kind, resource_id, reference_kind, reference_id
          ) VALUES (
            ${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(lateReference))},
            'session_binding', 'late-session-reference'
          );
        END;
        CREATE TRIGGER reject_release
          BEFORE DELETE ON resource_lifecycle_reclamation_fences
          WHEN OLD.resource_id = ${sqlQuote(blob(releaseFailure))}
        BEGIN
          SELECT RAISE(ABORT, 'injected release failure');
        END;
        CREATE TRIGGER inject_late_guard_error
          AFTER INSERT ON resource_lifecycle_reclamation_fences
          WHEN NEW.resource_id = ${sqlQuote(blob(lateGuardError))}
        BEGIN
          INSERT INTO resource_lifecycle_references(
            workspace_id, resource_kind, resource_id, reference_kind, reference_id
          ) VALUES (
            ${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(blob(lateGuardError))},
            'future_unknown_kind', 'late-corrupt-reference'
          );
        END;
        CREATE TRIGGER reject_combined_release
          BEFORE DELETE ON resource_lifecycle_reclamation_fences
          WHEN OLD.resource_id = ${sqlQuote(blob(combinedFailure))}
        BEGIN
          SELECT RAISE(ABORT, 'injected combined release failure');
        END;
        CREATE TRIGGER ignore_claim_save
          BEFORE UPDATE ON resource_lifecycle_purge_intents
          WHEN OLD.intent_id = ${sqlQuote(saveConflictIntent.intent_id)}
        BEGIN
          SELECT RAISE(IGNORE);
        END;
        CREATE TRIGGER reject_claim_save
          BEFORE UPDATE ON resource_lifecycle_purge_intents
          WHEN OLD.intent_id = ${sqlQuote(saveFailureIntent.intent_id)}
        BEGIN
          SELECT RAISE(ABORT, 'injected claim save failure');
        END;
      `,
    );
    sqlite(
      files,
      `
        CREATE TRIGGER reject_physical_purge
          BEFORE DELETE ON file_store_blob
          WHEN OLD.id = ${sqlQuote(blob(physicalFailure))}
        BEGIN
          SELECT RAISE(ABORT, 'injected physical purge failure');
        END;
        CREATE TRIGGER reject_combined_physical_purge
          BEFORE DELETE ON file_store_blob
          WHEN OLD.id = ${sqlQuote(blob(combinedFailure))}
        BEGIN
          SELECT RAISE(ABORT, 'injected combined physical purge failure');
        END;
      `,
    );

    server = start(directory, { captureStderr: true });
    await ready(server);
    await waitForStderr(server, /injected claim save failure/u);
    assert.equal(intentFor(directory, saveConflict).attempts, 0);
    assert.equal(intentFor(directory, saveFailure).attempts, 0);
    sqlite(
      lifecycle,
      `DROP TRIGGER ignore_claim_save; DROP TRIGGER reject_claim_save;`,
    );

    // Cause/effect graph: C1 same-workspace admission succeeds; C2 the File has
    // an active physical-reclamation fence; C3 the Session has no prior binding.
    // C1 && C2 && C3 -> E1 retryable 503/api_error, E2 the active manifest stays
    // empty, E3 no durable session_binding reference is created. Decision-table
    // rule R1=[C1=T,C2=T,C3=T]=>[E1,E2,E3]. The fence is an intrinsic resource
    // consistency boundary, not an IAM decision. FMECA: reporting 500 hides the
    // retry contract; accepting the binding races reclamation and can expose a
    // missing blob. The status plus both rollback assertions detect either mode.
    const fencedSession = await json('POST', 'sessions', {
      agent: 'assistant',
      environment_id: 'env_local',
    });
    assert.equal(fencedSession.status, 200, JSON.stringify(fencedSession.body));
    sqlite(
      lifecycle,
      `INSERT INTO resource_lifecycle_reclamation_fences(resource_kind, resource_id, intent_id)
         VALUES ('file', ${sqlQuote(blob(fencedBinding))}, 'external-active-fence');`,
    );
    const fencedActivation = await json(
      'POST',
      `sessions/${fencedSession.body.id}/resources`,
      { type: 'file', file_id: fencedBinding, mount_path: '/workspace/fenced.txt' },
    );
    assert.equal(fencedActivation.status, 503, JSON.stringify(fencedActivation.body));
    assert.match(JSON.stringify(fencedActivation.body), /fenced for physical reclamation/u);
    const projected = await json('GET', `sessions/${fencedSession.body.id}/resources`);
    assert.equal(projected.status, 200, JSON.stringify(projected.body));
    assert.deepEqual(projected.body.data, []);
    assert.equal(
      Number(sqliteScalar(
        lifecycle,
        `SELECT count(*) FROM resource_lifecycle_references
          WHERE resource_kind = 'file' AND resource_id = ${sqlQuote(blob(fencedBinding))}
            AND reference_kind = 'session_binding';`,
      )),
      0,
    );
    sqlite(
      lifecycle,
      `DELETE FROM resource_lifecycle_reclamation_fences
        WHERE resource_kind = 'file' AND resource_id = ${sqlQuote(blob(fencedBinding))}
          AND intent_id = 'external-active-fence';`,
    );

    // Live Repository attachment is not an activation path: the official
    // subresource is File-only. Admission must reject before catalog/runtime
    // effects, so no compensating reclamation intent is manufactured.
    const repositorySession = await json('POST', 'sessions', {
      agent: 'assistant',
      environment_id: 'env_local',
    });
    assert.equal(repositorySession.status, 200, JSON.stringify(repositorySession.body));
    const repositoryIntentsBefore = intents(directory)
      .filter((intent) => intent.target.kind === 'repository').length;
    const repositoryActivation = await json(
      'POST',
      `sessions/${repositorySession.body.id}/resources`,
      {
        type: 'github_repository',
        url: 'https://example.invalid/reclamation-fixture.git',
        mount_path: '/workspace/fenced-repository',
      },
    );
    assert.equal(repositoryActivation.status, 400, JSON.stringify(repositoryActivation.body));
    assert.deepEqual(
      (await json('GET', `sessions/${repositorySession.body.id}/resources`)).body.data,
      [],
    );
    assert.equal(
      intents(directory).filter((intent) => intent.target.kind === 'repository').length,
      repositoryIntentsBefore,
      'typed admission failure creates no Repository lifecycle side effect',
    );

    // Keep the tombstoned Skill valid while unrelated File/Repository Sessions
    // are created, then inject the storage fault and release its foreign fence so
    // the recurring reclaimer—not Session setup—observes the corruption.
    fs.writeFileSync(skillAggregate, '{broken-skill-aggregate');
    sqlite(
      lifecycle,
      `DELETE FROM resource_lifecycle_reclamation_fences
        WHERE resource_kind = 'skill' AND resource_id = ${sqlQuote(skillId)}
          AND intent_id = 'external-skill-reclaimer';`,
    );

    const failed = await waitFor(
      directory,
      [
        releaseFailure,
        contended,
        lateReference,
        durableBlockers,
        corruptReference,
        physicalFailure,
        combinedFailure,
        lateGuardError,
        skillId,
      ],
      (intent) => intent.status === 'pending'
        && intent.attempts >= 1
        && (intent.target.resource_id !== skillId
          || /expected value|key must be a string/u.test(intent.last_error)),
    );
    const byResource = new Map(failed.map((intent) => [logicalResourceId(intent), intent]));
    assert.match(byResource.get(releaseFailure).last_error, /injected release failure/u);
    assert.match(byResource.get(contended).last_error, /fenced by another reclamation intent/u);
    assert.ok(
      byResource.get(lateReference).blockers.some(
        (blocker) => blocker.reference_id === 'late-session-reference',
      ),
    );
    assert.match(byResource.get(skillId).last_error, /expected value|key must be a string/u);
    assert.deepEqual(
      byResource.get(durableBlockers).blockers.map((blocker) => blocker.kind).sort(),
      [
        'agent_binding',
        'artifact',
        'extraction_intent',
        'logical_lifecycle',
        'retention_hold',
        'runtime_handle',
        'workspace_ownership',
      ],
    );
    assert.match(byResource.get(corruptReference).last_error, /unknown resource reference kind/u);
    assert.match(byResource.get(physicalFailure).last_error, /injected physical purge failure/u);
    assert.match(
      byResource.get(combinedFailure).last_error,
      /injected combined physical purge failure; failed to release reclamation fence:.*injected combined release failure/u,
    );
    assert.match(byResource.get(lateGuardError).last_error, /unknown resource reference kind/u);

    // Remove only the injected faults. The coordinator must reuse each durable
    // intent/fence and complete; no API delete is repeated.
    fs.writeFileSync(skillAggregate, tombstone);
    sqlite(
      lifecycle,
      `
        DROP TRIGGER inject_late_reference;
        DROP TRIGGER reject_release;
        DROP TRIGGER inject_late_guard_error;
        DROP TRIGGER reject_combined_release;
        DELETE FROM resource_lifecycle_references
          WHERE resource_id = ${sqlQuote(blob(lateReference))}
            AND reference_id = 'late-session-reference';
        DELETE FROM resource_lifecycle_references
          WHERE resource_id IN (${sqlQuote(blob(durableBlockers))}, ${sqlQuote(blob(corruptReference))});
        DELETE FROM resource_lifecycle_references
          WHERE resource_id = ${sqlQuote(blob(lateGuardError))}
            AND reference_id = 'late-corrupt-reference';
        DELETE FROM resource_lifecycle_reclamation_fences
          WHERE resource_id = ${sqlQuote(blob(contended))}
            AND intent_id = 'external-reclaimer';
      `,
    );
    sqlite(
      files,
      `DROP TRIGGER reject_physical_purge; DROP TRIGGER reject_combined_physical_purge;`,
    );
    const completed = await waitFor(
      directory,
      [
        releaseFailure,
        contended,
        lateReference,
        durableBlockers,
        corruptReference,
        alreadyOwned,
        physicalFailure,
        combinedFailure,
        lateGuardError,
        saveConflict,
        saveFailure,
        skillId,
      ],
      (intent) => intent.status === 'completed',
    );
    assert.ok(completed.every((intent) => intent.receipt !== null));
    assert.equal(byResource.get(releaseFailure).attempts + 1, intentFor(directory, releaseFailure).attempts);
    assert.equal(intentFor(directory, releaseFailure).receipt.evidence.blob_deleted, false);
    assert.equal(intentFor(directory, skillId).receipt.evidence.versions_deleted, 1);

    // A durable adapter must not deserialize malformed-but-well-typed lifecycle
    // state and continue reclaiming. Exercise each invariant through the real
    // recurring reconciler (no test endpoint): the row remains pending and the
    // process reports the precise fail-closed reason. Restore the completed row
    // afterwards so this fault campaign itself leaves a converged catalog.
    const completedIntent = intentFor(directory, releaseFailure);
    await stop(server, 'SIGKILL');
    const pending = {
      ...completedIntent,
      status: 'pending',
      receipt: null,
      claim_owner: null,
      lease_expires_at_unix_ms: null,
      blockers: [],
    };
    const corruptions = [
      [{ ...pending, target: { ...pending.target, workspace_id: ' ' } }, /target workspace_id must not be empty/u],
      [{ ...pending, intent_id: ' ' }, /intent_id must not be empty/u],
      [{ ...pending, config_version: 0 }, /config_version must be positive/u],
      [{ ...pending, not_before_unix_ms: pending.requested_at_unix_ms - 1 }, /not_before_unix_ms precedes/u],
      [{
        ...pending,
        blockers: [{ kind: 'artifact', reference_id: ' ' }],
      }, /reference_id must not be empty/u],
    ];
    server = start(directory, { captureStderr: true });
    await ready(server);
    for (const [corrupt, expected] of corruptions) {
      server.stderrText = '';
      sqlite(
        lifecycle,
        `UPDATE resource_lifecycle_purge_intents
           SET status = 'pending', not_before_unix_ms = 0,
               lease_expires_at_unix_ms = NULL, data = ${sqlQuote(JSON.stringify(corrupt))}
         WHERE intent_id = ${sqlQuote(completedIntent.intent_id)};`,
      );
      await waitForStderr(server, expected);
      assert.equal(intentFor(directory, releaseFailure).status, 'pending');
    }
    sqlite(
      lifecycle,
      `UPDATE resource_lifecycle_purge_intents
         SET status = 'completed',
             not_before_unix_ms = ${completedIntent.not_before_unix_ms},
             lease_expires_at_unix_ms = NULL,
             data = ${sqlQuote(JSON.stringify(completedIntent))}
       WHERE intent_id = ${sqlQuote(completedIntent.intent_id)};`,
    );

    console.log('E2E PASS: reclaimer faults remain fenced, retryable, and idempotently convergent.');
  } finally {
    await stop(server).catch(() => {});
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
