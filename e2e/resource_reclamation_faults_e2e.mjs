// Recoverable-fault E2E for the authorization-agnostic ResourceReclaimer.
// Durable SQLite faults model process/race boundaries without adding test-only
// HTTP hooks to production services.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 38440);
const WORKSPACE = `reclamation-faults-${process.pid}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function binary() {
  const output = execSync('cargo build --quiet --message-format=json -p awaken-cli --bin awaken', {
    cwd: ROOT,
    maxBuffer: 64 * 1024 * 1024,
  }).toString();
  for (const line of output.split('\n')) {
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch { /* cargo diagnostic */ }
  }
  throw new Error('awaken binary was not produced');
}

function start(bin, directory, { captureStderr = false } = {}) {
  const child = spawn(bin, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${PORT}`,
      AWAKEN_LOCAL_WORKSPACE_ID: WORKSPACE,
      AWAKEN_STORAGE_DIR: directory,
      AWAKEN_DEPLOYMENT_DATA_DIR: directory,
      AWAKEN_MGMT_SEAL_KEY: '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff',
    },
    stdio: ['ignore', 'ignore', captureStderr ? 'pipe' : 'inherit'],
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
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const connected = await new Promise((resolve) => {
      const socket = net.createConnection({ port: PORT, host: '127.0.0.1' });
      socket.once('connect', () => { socket.destroy(); resolve(true); });
      socket.once('error', () => { socket.destroy(); resolve(false); });
    });
    if (connected) return;
    if (child.exitCode !== null) throw new Error(`awaken exited with ${child.exitCode}`);
    await sleep(100);
  }
  throw new Error('awaken did not become ready');
}

async function stop(child, signal = 'SIGINT') {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = new Promise((resolve) => child.once('exit', resolve));
  child.kill(signal);
  await exited;
}

const scoped = (tail) =>
  `http://127.0.0.1:${PORT}/v1/workspaces/${WORKSPACE}/${tail}`;

async function json(method, tail, body) {
  const response = await fetch(scoped(tail), {
    method,
    headers: body === undefined ? {} : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function upload(content, filename) {
  const form = new FormData();
  form.append('purpose', 'agent');
  form.append('file', new Blob([content]), filename);
  const response = await fetch(scoped('files'), { method: 'POST', body: form });
  assert.equal(response.status, 200);
  return (await response.json()).id;
}

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return execFileSync('sqlite3', [database], { input: sql, encoding: 'utf8' });
}

function intents(directory) {
  const database = path.join(directory, 'resource-lifecycle.db');
  const output = execFileSync('sqlite3', [
    '-json',
    database,
    'SELECT data FROM resource_purge_intents ORDER BY intent_id',
  ]).toString().trim();
  return output ? JSON.parse(output).map((row) => JSON.parse(row.data)) : [];
}

function intentFor(directory, resourceId) {
  return intents(directory).find((intent) => intent.target.resource_id === resourceId);
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
  const lifecycle = path.join(directory, 'resource-lifecycle.db');
  const bin = binary();
  let server = start(bin, directory);
  try {
    await ready(server);
    const releaseFailure = await upload('release-failure', 'release.txt');
    const contended = await upload('contended-fence', 'contended.txt');
    const lateReference = await upload('late-reference', 'late.txt');
    const durableBlockers = await upload('durable-blockers', 'blockers.txt');
    const corruptReference = await upload('corrupt-reference', 'corrupt-reference.txt');
    const alreadyOwned = await upload('already-owned-fence', 'already-owned.txt');
    const fencedBinding = await upload('fenced-binding', 'fenced-binding.txt');
    const skillId = `fault-skill-${process.pid}`;
    assert.equal((await json('POST', 'skills', {
      id: skillId,
      content: `---\nname: ${skillId}\ndescription: fault recovery\n---\nRecover safely.`,
    })).status, 200);

    for (const fileId of [
      releaseFailure,
      contended,
      lateReference,
      durableBlockers,
      corruptReference,
      alreadyOwned,
    ]) {
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
    fs.writeFileSync(skillAggregate, '{broken-skill-aggregate');
    const alreadyOwnedIntent = intentFor(directory, alreadyOwned);
    assert.ok(alreadyOwnedIntent, 'the logical delete durably scheduled reclamation');

    // The three triggers/rows model distinct production races at durable seams:
    // a foreign reclaimer already owns one identity; a reference appears after
    // the first guard scan; and release fails after idempotent physical deletion.
    sqlite(
      lifecycle,
      `
        INSERT INTO resource_reclamation_fences(resource_kind, resource_id, intent_id)
          VALUES ('file', ${sqlQuote(contended)}, 'external-reclaimer');
        INSERT INTO resource_reclamation_fences(resource_kind, resource_id, intent_id)
          VALUES ('file', ${sqlQuote(alreadyOwned)}, ${sqlQuote(alreadyOwnedIntent.intent_id)});
        INSERT INTO resource_references(
          workspace_id, resource_kind, resource_id, reference_kind, reference_id
        ) VALUES
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'logical_lifecycle', 'logical-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'workspace_ownership', 'owner-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'agent_binding', 'agent-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'artifact', 'artifact-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'runtime_handle', 'runtime-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'extraction_intent', 'extract-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(durableBlockers)}, 'retention_hold', 'hold-1'),
          (${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(corruptReference)}, 'future_unknown_kind', 'bad-1');
        CREATE TRIGGER inject_late_reference
          AFTER INSERT ON resource_reclamation_fences
          WHEN NEW.resource_id = ${sqlQuote(lateReference)}
        BEGIN
          INSERT INTO resource_references(
            workspace_id, resource_kind, resource_id, reference_kind, reference_id
          ) VALUES (
            ${sqlQuote(WORKSPACE)}, 'file', ${sqlQuote(lateReference)},
            'session_binding', 'late-session-reference'
          );
        END;
        CREATE TRIGGER reject_release
          BEFORE DELETE ON resource_reclamation_fences
          WHEN OLD.resource_id = ${sqlQuote(releaseFailure)}
        BEGIN
          SELECT RAISE(ABORT, 'injected release failure');
        END;
      `,
    );

    server = start(bin, directory);
    await ready(server);

    // A reclamation fence is an intrinsic resource consistency boundary, not an
    // IAM decision. Even after the API edge admitted this same-workspace request,
    // the durable adapter must reject a new Session reference and activation must
    // roll back to the prior empty manifest.
    const fencedSession = await json('POST', 'sessions', {
      agent: 'assistant',
      environment_id: 'env_local',
    });
    assert.equal(fencedSession.status, 200, JSON.stringify(fencedSession.body));
    sqlite(
      lifecycle,
      `INSERT INTO resource_reclamation_fences(resource_kind, resource_id, intent_id)
         VALUES ('file', ${sqlQuote(fencedBinding)}, 'external-active-fence');`,
    );
    const fencedActivation = await json(
      'POST',
      `sessions/${fencedSession.body.id}/resources`,
      { type: 'file', file_id: fencedBinding, mount_path: '/workspace/fenced.txt' },
    );
    assert.equal(fencedActivation.status, 500, JSON.stringify(fencedActivation.body));
    assert.match(JSON.stringify(fencedActivation.body), /fenced for physical reclamation/u);
    const projected = await json('GET', `sessions/${fencedSession.body.id}/resources`);
    assert.equal(projected.status, 200, JSON.stringify(projected.body));
    assert.deepEqual(projected.body.data, []);
    assert.equal(
      sqlite(
        lifecycle,
        `SELECT count(*) FROM resource_references
          WHERE resource_kind = 'file' AND resource_id = ${sqlQuote(fencedBinding)}
            AND reference_kind = 'session_binding';`,
      ).trim(),
      '0',
    );
    sqlite(
      lifecycle,
      `DELETE FROM resource_reclamation_fences
        WHERE resource_kind = 'file' AND resource_id = ${sqlQuote(fencedBinding)}
          AND intent_id = 'external-active-fence';`,
    );

    const failed = await waitFor(
      directory,
      [releaseFailure, contended, lateReference, durableBlockers, corruptReference, skillId],
      (intent) => intent.status === 'pending' && intent.attempts >= 1,
    );
    const byResource = new Map(failed.map((intent) => [intent.target.resource_id, intent]));
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

    // Remove only the injected faults. The coordinator must reuse each durable
    // intent/fence and complete; no API delete is repeated.
    fs.writeFileSync(skillAggregate, tombstone);
    sqlite(
      lifecycle,
      `
        DROP TRIGGER inject_late_reference;
        DROP TRIGGER reject_release;
        DELETE FROM resource_references
          WHERE resource_id = ${sqlQuote(lateReference)}
            AND reference_id = 'late-session-reference';
        DELETE FROM resource_references
          WHERE resource_id IN (${sqlQuote(durableBlockers)}, ${sqlQuote(corruptReference)});
        DELETE FROM resource_reclamation_fences
          WHERE resource_id = ${sqlQuote(contended)}
            AND intent_id = 'external-reclaimer';
      `,
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
    server = start(bin, directory, { captureStderr: true });
    await ready(server);
    for (const [corrupt, expected] of corruptions) {
      server.stderrText = '';
      sqlite(
        lifecycle,
        `UPDATE resource_purge_intents
           SET status = 'pending', not_before_unix_ms = 0,
               lease_expires_at_unix_ms = NULL, data = ${sqlQuote(JSON.stringify(corrupt))}
         WHERE intent_id = ${sqlQuote(completedIntent.intent_id)};`,
      );
      await waitForStderr(server, expected);
      assert.equal(intentFor(directory, releaseFailure).status, 'pending');
    }
    sqlite(
      lifecycle,
      `UPDATE resource_purge_intents
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
