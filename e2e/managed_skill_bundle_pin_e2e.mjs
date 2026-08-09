// Durable Skill bundle + Session pin end to end.
//
// This drives the production management composition: the edge resolves the
// Workspace and enforces resource actions, while the Skill repository sees only
// that trusted Workspace coordinate. It proves binary-safe bundles, one-time
// Session resolution, logical version retirement, and restart rehydration.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { pass, sendAndListNewEvents, spawnServer, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38237);
const BETAS = ['managed-agents-2026-04-01'];
const SKILLS_BETA = 'skills-2025-10-02';
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const AGENT = 'skill-pin-agent';
const V1 = 'PINNED_SKILL_V1_7119';
const V2 = 'CURRENT_SKILL_V2_8120';
const BINARY = Uint8Array.from([0, 159, 146, 150, 255, 13, 0, 10]);
let adminToken = '';

function skillMarkdown(marker) {
  return `---\nname: greet\ndescription: deterministic pinned skill\n---\n${marker}`;
}

async function request(baseUrl, method, route, body) {
  const skillHeaders = route === '/v1/skills' || route.startsWith('/v1/skills/')
    ? { 'anthropic-beta': SKILLS_BETA }
    : {};
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: {
      ...skillHeaders,
      authorization: `Bearer ${adminToken}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const bytes = new Uint8Array(await response.arrayBuffer());
  const text = new TextDecoder().decode(bytes);
  let value = text;
  try { value = JSON.parse(text); } catch {}
  return { status: response.status, body: value, bytes };
}

async function uploadBundle(baseUrl, route, marker, binary = undefined) {
  const form = new FormData();
  form.append('file', new Blob([skillMarkdown(marker)], { type: 'text/markdown' }), 'SKILL.md');
  if (binary !== undefined) {
    form.append('file', new Blob([binary], { type: 'application/octet-stream' }), 'assets/data.bin');
  }
  const response = await fetch(`${baseUrl}${route}`, {
    method: 'POST',
    headers: {
      'anthropic-beta': SKILLS_BETA,
      authorization: `Bearer ${adminToken}`,
    },
    body: form,
  });
  const body = await response.json().catch(() => ({}));
  assert.equal(response.status, 200, `${route}: ${JSON.stringify(body)}`);
  return body;
}

async function publishAgent(baseUrl, skillId) {
  const config = {
    id: AGENT,
    system: 'Use the selected skill.',
    max_steps: 8,
    model: { id: 'management-skills' },
    tools: [],
    plugins: [],
    plugin_config: {},
    skills: [{ id: skillId }],
  };
  const put = await request(baseUrl, 'PUT', `/v1/config/agents/${AGENT}`, config);
  assert.equal(put.status, 200, `agent config stored: ${JSON.stringify(put.body)}`);
  const published = await request(baseUrl, 'POST', `/v1/config/agents/${AGENT}/publish`);
  assert.equal(published.status, 200, `agent config published: ${JSON.stringify(published.body)}`);
  assert.equal(published.body.installed, true, 'agent publication installed');
}

async function createSession(client, skillSelection = undefined) {
  return client.beta.sessions.create({
    agent: skillSelection === undefined ? AGENT : {
      id: AGENT,
      type: 'agent_with_overrides',
      skills: skillSelection,
    },
    environment_id: 'env_local',
    betas: BETAS,
  });
}

async function runAndReadLastReply(client, sessionId, text) {
  const events = await sendAndListNewEvents(client, sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const replies = events.filter((event) => event.type === 'agent.message');
  assert.ok(replies.length > 0, `session emitted a new agent message for ${JSON.stringify(text)}`);
  return JSON.stringify(replies.at(-1).content);
}

function onlySkillAggregate(storageDir) {
  const root = path.join(storageDir, 'skills');
  const files = fs.readdirSync(root, { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .flatMap((workspace) => fs.readdirSync(path.join(root, workspace.name), { withFileTypes: true })
      .filter((entry) => entry.isFile() && entry.name.endsWith('.json'))
      .map((entry) => path.join(root, workspace.name, entry.name)));
  assert.equal(files.length, 1, `one durable Skill aggregate exists: ${JSON.stringify(files)}`);
  return files[0];
}

async function assertCorruptAggregateRejected(baseUrl, aggregatePath, clean, label, mutate) {
  const damaged = structuredClone(clean);
  mutate(damaged);
  fs.writeFileSync(aggregatePath, JSON.stringify(damaged));
  try {
    const response = await request(baseUrl, 'GET', `/v1/skills/${clean.definition.id}/versions/2`);
    assert.equal(response.status, 500, `${label} fails closed: ${JSON.stringify(response.body)}`);
    assert.match(
      JSON.stringify(response.body),
      /invalid persisted Skill aggregate/u,
      `${label} reports repository corruption`,
    );
  } finally {
    fs.writeFileSync(aggregatePath, JSON.stringify(clean));
  }
}

async function main() {
  const managementDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-skill-pin-mgmt-'));
  const homeDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-skill-pin-home-'));
  const configDir = path.join(homeDir, '.awaken');
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(path.join(configDir, 'config.toml'), [
    `data_dir = ${JSON.stringify(managementDir)}`,
    `control_seal_key = ${JSON.stringify(SEAL_KEY)}`,
    'sandbox_tier = "local"',
    '',
  ].join('\n'));
  const env = {
    HOME: homeDir,
  };
  let server = null;
  try {
    // Lifetime A: publish v1, freeze it into a Session, then publish v2.
    const first = spawnServer('management-skills', PORT, env);
    server = first.server;
    await waitForPort(PORT);
    adminToken = fs.readFileSync(path.join(managementDir, 'admin-token'), 'utf8').trim();
    let client = new Anthropic({ apiKey: adminToken, baseURL: first.baseUrl });

    const created = await uploadBundle(first.baseUrl, '/v1/skills', V1, BINARY);
    const skillId = created.id;
    assert.match(skillId, /^skill_[0-9a-f]{16}$/u, 'multipart upload returned a stable catalog id');

    const binary = await request(
      first.baseUrl,
      'GET',
      `/v1/skills/${skillId}/versions/1/files/assets/data.bin`,
    );
    assert.equal(binary.status, 200, 'support file is retrievable');
    assert.deepEqual(binary.bytes, BINARY, 'support file is binary-safe');
    pass('full binary Skill bundle persisted without UTF-8 coercion');

    await publishAgent(first.baseUrl, skillId);
    const pinned = await createSession(client);
    const firstReply = await runAndReadLastReply(client, pinned.id, 'use the frozen skill');
    assert.ok(firstReply.includes(V1), `the Session resolved Skill v1: ${firstReply}`);

    const version2 = await uploadBundle(
      first.baseUrl,
      `/v1/skills/${skillId}/versions`,
      V2,
    );
    assert.equal(version2.version, '2', 'v2 appended monotonically');

    // Causes: the published Agent selects `latest`, while a create-time override
    // selects exact v1 after v2 exists. Constraint: the override replaces the
    // root Agent list and resolution occurs once before preparation. Effects:
    // P1 latest -> v2; P2 exact "1" -> v1; both persist immutable pins.
    const exactV1 = await createSession(client, [{
      type: 'custom', skill_id: skillId, version: '1',
    }]);
    const exactReply = await runAndReadLastReply(client, exactV1.id, 'use exact version one');
    assert.ok(exactReply.includes(V1), `P2 exact selector resolved v1 after v2 existed: ${exactReply}`);
    assert.ok(!exactReply.includes(V2), 'P2 exact selector did not drift to latest');
    pass('create-time exact custom Skill selector replaces latest and pins v1');

    const retired = await request(
      first.baseUrl,
      'DELETE',
      `/v1/skills/${skillId}/versions/1`,
    );
    assert.equal(retired.status, 200, `v1 retired: ${JSON.stringify(retired.body)}`);
    const hidden = await request(first.baseUrl, 'GET', `/v1/skills/${skillId}/versions/1`);
    assert.equal(hidden.status, 404, 'retired v1 is hidden from the management API');
    pass('v1 retired from management views after v2 publication');

    // Lifetime B: old Session reloads its durable v1 pin; a new Session selects v2.
    await stopServer(first.server);
    server = null;
    const second = spawnServer('management-skills', PORT, env);
    server = second.server;
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: adminToken, baseURL: second.baseUrl });

    const restoredReply = await runAndReadLastReply(client, pinned.id, 'use it again after restart');
    assert.ok(restoredReply.includes(V1), 'rehydrated old Session still loads retained v1 bytes');
    assert.ok(!restoredReply.includes(V2), 'old Session did not drift to current v2');

    const current = await createSession(client);
    const currentReply = await runAndReadLastReply(client, current.id, 'use the current skill');
    assert.ok(currentReply.includes(V2), 'new Session resolves current v2');
    assert.ok(!currentReply.includes(V1), 'new Session does not select retired v1');
    pass('restart preserves old Session pin while a new Session selects v2');

    // The repository must reject damaged durable state before either its API or
    // runtime can project it. Exercise every persisted aggregate invariant through
    // the real HTTP composition, restoring the valid bytes between faults.
    const aggregatePath = onlySkillAggregate(managementDir);
    const cleanAggregate = JSON.parse(fs.readFileSync(aggregatePath, 'utf8'));
    const corruptions = [
      ['Workspace identity mismatch', (value) => { value.definition.workspace_id = 'forged'; }],
      ['Skill identity mismatch', (value) => { value.definition.id = 'forged'; }],
      ['empty version history', (value) => { value.versions = {}; }],
      ['last version drift', (value) => { value.definition.last_version = 99; }],
      ['unknown retired version', (value) => { value.retired_versions.push(99); }],
      ['no visible version', (value) => { value.retired_versions = [1, 2]; }],
      ['latest version drift', (value) => { value.definition.latest_version = 1; }],
      ['zero version', (value) => {
        value.versions = { 0: { ...value.versions['2'], version: 0 }, 1: value.versions['1'] };
        value.definition.last_version = 1;
        value.definition.latest_version = 0;
      }],
      ['version key drift', (value) => { value.versions['2'].version = 1; }],
      ['version owner drift', (value) => { value.versions['2'].skill_id = 'forged'; }],
      ['empty bundle', (value) => { value.versions['2'].files = []; }],
      ['missing SKILL.md', (value) => { value.versions['2'].files[0].path = 'README.md'; }],
      ['bundle content changed', (value) => {
        const content = value.versions['2'].files[0].content;
        content[content.length - 1] ^= 1;
      }],
    ];
    for (const [label, mutate] of corruptions) {
      await assertCorruptAggregateRejected(
        second.baseUrl,
        aggregatePath,
        cleanAggregate,
        label,
        mutate,
      );
    }
    fs.writeFileSync(aggregatePath, '{');
    try {
      const malformed = await request(second.baseUrl, 'GET', `/v1/skills/${skillId}/versions/2`);
      assert.equal(malformed.status, 500, 'malformed aggregate JSON fails closed');
      assert.match(JSON.stringify(malformed.body), /malformed JSON/u);
    } finally {
      fs.writeFileSync(aggregatePath, JSON.stringify(cleanAggregate));
    }
    const forgedPath = path.join(path.dirname(aggregatePath), '00.json');
    fs.writeFileSync(forgedPath, JSON.stringify(cleanAggregate));
    try {
      const poisonedList = await request(second.baseUrl, 'GET', '/v1/skills');
      assert.equal(poisonedList.status, 500, 'aggregate under a forged filesystem key fails closed');
      assert.match(JSON.stringify(poisonedList.body), /filesystem key/u);
    } finally {
      fs.rmSync(forgedPath, { force: true });
    }
    pass('all durable Skill aggregate corruption is rejected at the repository boundary');

    // A cold process must not trust a stale in-memory catalog. Cause/effect table:
    // C1 immutable stored bytes are corrupt; C2 request uses a retained exact pin
    // or performs a fresh resolution. C1+C2 => E1 fail before model execution,
    // E2 preserve the storage-corruption 500 taxonomy, and E3 never drift to
    // another version. Invalid client input would be 400; this is durable damage.
    await stopServer(second.server);
    server = null;
    const damaged = structuredClone(cleanAggregate);
    const retainedContent = damaged.versions['1'].files[0].content;
    retainedContent[retainedContent.length - 1] ^= 1;
    fs.writeFileSync(aggregatePath, JSON.stringify(damaged));
    const third = spawnServer('management-skills', PORT, env);
    server = third.server;
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: adminToken, baseURL: third.baseUrl });
    await assert.rejects(
      runAndReadLastReply(client, pinned.id, 'do not run a corrupted retained Skill'),
      (error) => {
        assert.equal(error.status, 500, `retained Session corruption is a client-visible rejection: ${error}`);
        assert.match(error.message, /invalid persisted Skill aggregate/u);
        return true;
      },
    );
    await assert.rejects(
      createSession(client),
      (error) => {
        assert.equal(error.status, 500, `fresh resolution corruption is rejected: ${error}`);
        assert.match(error.message, /invalid persisted Skill aggregate/u);
        return true;
      },
    );
    pass('restart cannot bypass bundle integrity for retained or newly resolved Sessions');

    console.log('E2E PASS: binary bundle + exact pin + restart and corruption fail-closed.');
  } finally {
    if (server) await stopServer(server);
    fs.rmSync(managementDir, { recursive: true, force: true });
    fs.rmSync(homeDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
