// One-time resource-plane upgrade through the real server process.
//
// The former HTTP-local registry stored Memory version rows and Skill records in
// `resource-api.db`. Current code imports those rows into the canonical Memory and
// Skill aggregates exactly once. This scenario creates the old database, starts a
// server, observes the upgraded resources, removes the old database, and starts a
// replacement server to prove there is no continuing dual-read dependency.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38641);
const BETAS = ['managed-agents-2026-04-01'];

function sqlQuote(value) {
  return `'${String(value).replaceAll("'", "''")}'`;
}

function sqlite(database, sql) {
  return execFileSync('sqlite3', [database], { input: sql, encoding: 'utf8' });
}

function sqliteJson(database, sql) {
  const output = execFileSync('sqlite3', ['-json', database, sql], { encoding: 'utf8' });
  return output.trim() ? JSON.parse(output) : [];
}

function seedLegacyDatabase(database) {
  const memoryVersion = JSON.stringify({
    id: 'memver_0000000000000042',
    memory_id: 'mem_legacy',
    operation: 'created',
    content: 'legacy-memory-content',
    path: '/legacy.md',
    redacted_at: null,
  });
  const skill = JSON.stringify({
    display_title: 'Imported legacy skill',
    versions: [
      {
        id: 'skver_legacy_1',
        version: '1',
        name: 'legacy-name',
        description: 'first legacy version',
        directory: '/skills/legacy-name',
        content: '---\nname: legacy-name\ndescription: first legacy version\n---\nLEGACY_V1',
        files: {},
      },
      {
        id: 'skver_legacy_2',
        version: '2',
        name: 'legacy-name',
        description: 'second legacy version',
        directory: '/skills/legacy-name',
        content: 'unused fallback',
        files: {
          'SKILL.md':
            '---\nname: legacy-name\ndescription: second legacy version\n---\nLEGACY_V2',
          'references/info.md': 'legacy-support-file',
        },
      },
    ],
  });
  const invalidOrdinal = JSON.stringify({
    display_title: null,
    versions: [
      {
        id: 'skver_invalid_5',
        version: '5',
        name: 'invalid-order',
        description: 'must be skipped',
        directory: '/skills/invalid-order',
        content: '# invalid',
        files: {},
      },
    ],
  });
  sqlite(
    database,
    `
      CREATE TABLE memory_versions (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        store_id TEXT NOT NULL,
        version_id TEXT UNIQUE,
        data TEXT NOT NULL
      );
      CREATE TABLE skill_records (
        workspace_id TEXT NOT NULL,
        skill_id TEXT NOT NULL,
        data TEXT NOT NULL,
        PRIMARY KEY (workspace_id, skill_id)
      );
      INSERT INTO memory_versions(seq, store_id, version_id, data)
        VALUES (42, 'legacy-store', 'memver_0000000000000042', ${sqlQuote(memoryVersion)});
      INSERT INTO skill_records(workspace_id, skill_id, data)
        VALUES ('default', 'legacy-skill', ${sqlQuote(skill)});
      INSERT INTO skill_records(workspace_id, skill_id, data)
        VALUES ('default', 'invalid-order', ${sqlQuote(invalidOrdinal)});
    `,
  );
}

function client() {
  return new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
}

async function raw(pathname) {
  return fetch(`http://127.0.0.1:${PORT}${pathname}`, {
    headers: { 'x-api-key': 'e2e-dummy' },
  });
}

async function assertImportedSkill() {
  const c = client();
  const list = await c.get('/v1/skills');
  assert.deepEqual(
    list.data.map((entry) => entry.id),
    ['legacy-skill'],
    'only the sequential legacy aggregate is imported',
  );
  assert.equal(list.data[0].display_title, 'Imported legacy skill');
  assert.equal(list.data[0].latest_version, '2');

  // A second call in the same process exercises the one-shot Workspace guard and
  // must not duplicate versions.
  const versions = await c.get('/v1/skills/legacy-skill/versions');
  assert.deepEqual(
    versions.data.map((entry) => entry.version),
    ['1', '2'],
  );
  const latest = await c.get('/v1/skills/legacy-skill/versions/latest');
  assert.equal(latest.id, 'skver_legacy_2');

  const content = await raw('/v1/skills/legacy-skill/versions/2/content');
  assert.equal(content.status, 200);
  assert.match(await content.text(), /LEGACY_V2/);
  const support = await raw(
    '/v1/skills/legacy-skill/versions/2/files/references/info.md',
  );
  assert.equal(support.status, 200);
  assert.equal(await support.text(), 'legacy-support-file');
}

async function createMemory() {
  const c = client();
  const store = await c.beta.memoryStores.create({ betas: BETAS });
  const memory = await c.beta.memoryStores.memories.create(store.id, {
    path: '/new.md',
    content: 'new-memory',
    betas: BETAS,
  });
  assert.match(memory.memory_version_id, /^memver_mem_/);
}

async function main() {
  const storage = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-resource-upgrade-e2e-'));
  const legacy = path.join(storage, 'resource-api.db');
  seedLegacyDatabase(legacy);
  const upstream = await startUpstream('skills');
  const servers = [];
  const environment = {
    AWAKEN_STORAGE_DIR: storage,
    AWAKEN_LOCAL_WORKSPACE_ID: 'default',
    ...realServerEnv('skills', upstream, { mode: 'skills-durable' }),
  };

  try {
    const first = spawnServer('skills-durable', PORT, environment);
    servers.push(first.server);
    await waitForPort(PORT);
    await assertImportedSkill();
    await createMemory();
    pass('legacy Memory history and two-version Skill aggregate imported once');

    await stopServer(first.server);
    servers.pop();
    const importedRows = sqliteJson(
      path.join(storage, 'memory_fs.db'),
      'SELECT id, store_id, ordinal FROM memory_store_versions ORDER BY ordinal',
    );
    assert.equal(importedRows[0].id, 'memver_0000000000000042');
    assert.equal(importedRows[0].store_id, 'legacy-store');
    assert.deepEqual(
      importedRows.map((row) => row.ordinal),
      [42, 43],
      'legacy Memory ordinal advances the canonical version high-water mark',
    );

    // The old sidecar is no longer a runtime dependency after import.
    fs.rmSync(legacy);
    const second = spawnServer('skills-durable', PORT, environment);
    servers.push(second.server);
    await waitForPort(PORT);
    await assertImportedSkill();
    await createMemory();
    pass('replacement process rebuilt neither resource from the removed legacy database');

    await stopServer(second.server);
    servers.pop();
    assert.deepEqual(
      sqliteJson(
        path.join(storage, 'memory_fs.db'),
        'SELECT ordinal FROM memory_store_versions ORDER BY ordinal',
      ).map((row) => row.ordinal),
      [42, 43, 44],
      'replacement process continues from the canonical Memory counter',
    );

    console.log(
      'E2E PASS: legacy resource registry upgraded once into canonical Memory and Skill stores.',
    );
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
