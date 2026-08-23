// Runtime half of the registry-latest SDK canary. The declaration fingerprint
// detects shape drift; this executable smoke detects generated path, default
// beta, pagination and response-decoding drift against the real Awaken
// topologies that own each resource family.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import {
  pass,
  waitForSessionEventReceipt,
  withRealServer,
  withScenarioServer,
} from '../harness.mjs';

const packageRoot = process.env.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
assert.ok(packageRoot, 'ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT is required');
const manifest = JSON.parse(readFileSync(resolve(packageRoot, 'package.json'), 'utf8'));
const { default: Anthropic, toFile } = await import(pathToFileURL(resolve(packageRoot, 'index.mjs')));

await withRealServer('echo', 38190, async (baseURL) => {
  // Cause/effect graph: C0=Session Event send returns one exact durable
  // receipt; C1=the registry-latest generated Session and Memory
  // methods use their default beta/header behavior; C2=the echo topology owns
  // those resources. Effects are a decoded terminal Session stream plus Memory
  // CRUD round-trip. Decision rule R1: C0 && C1 && C2 => both resource families work;
  // any generated path/header/decoder drift fails at its official SDK call.
  // Constraints/invariant: the generated SDK methods and existing echo-owned
  // repositories are the only request/response paths; polling observes C0 and
  // cannot add a route, beta override, or second completion authority.
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
  });
  try {
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `latest-sdk-${manifest.version}` }],
      }],
    });
    const acceptedId = receipt.data[0]?.id;
    assert.equal(typeof acceptedId, 'string', 'R1 exact accepted User Event id');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      acceptedId,
      undefined,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      `registry SDK ${manifest.version} Session Run to settle`,
      { listParams: { limit: 1 } },
    );
    assert.ok(events.some((event) => event.type === 'agent.message'));
    assert.ok(events.some((event) => event.type === 'session.status_idle'));
  } finally {
    await client.beta.sessions.delete(session.id);
  }

  const store = await client.beta.memoryStores.create({ name: `latest-${manifest.version}` });
  const memory = await client.beta.memoryStores.memories.create(store.id, {
    path: '/latest.md',
    content: manifest.version,
    view: 'full',
  });
  assert.equal((await client.beta.memoryStores.memories.retrieve(memory.id, {
    memory_store_id: store.id,
  })).content, manifest.version);
  await client.beta.memoryStores.memories.delete(memory.id, { memory_store_id: store.id });
  await client.beta.memoryStores.archive(store.id);
  await client.beta.memoryStores.delete(store.id);

  // Cause/effect graph: C3=0.120 exposes GA Files and Skills outside `beta`;
  // C4=both project the existing FileCatalog/SkillStore; C5=GA Files expiry and
  // ids[] pagination, and GA Skill source/latest-version fields differ from beta.
  // Effects: E3 generated GA paths run without a beta header, E4 exact GA DTOs
  // decode, E5 the same created ids remain visible through their one repository.
  // Decision table: R3 C3+C4+C5 -> create/list/retrieve/delete both families;
  // any accidental beta projection, missing expiry, or duplicate store fails.
  const file = await client.files.upload({
    file: await toFile(Buffer.from(manifest.version), 'latest.txt'),
    expires_in_seconds: 3600,
  });
  assert.equal(file.type, 'file', 'R3/E3');
  assert.equal(typeof file.expires_at, 'string', 'R3/E4 expiry');
  const files = await client.files.list({ ids: [file.id, 'file_missing'] });
  assert.deepEqual(files.data.map((item) => item.id), [file.id], 'R3/E5 ids[]');
  assert.equal((await client.files.retrieveMetadata(file.id)).id, file.id);
  await client.files.delete(file.id);

  const skill = await client.skills.create({
    display_name: `Latest ${manifest.version}`,
    files: [await toFile(
      Buffer.from(`---\nname: latest-skill\ndescription: SDK ${manifest.version}\n---\n`),
      'latest-skill/SKILL.md',
    )],
  });
  assert.equal(skill.source.type, 'custom', 'R3/E4 source object');
  assert.equal(typeof skill.latest_version_id, 'string', 'R3/E4 version id');
  assert.equal((await client.skills.retrieve(skill.id)).id, skill.id);
  const skillPage = await client.skills.list({ source: 'custom' });
  assert.ok(skillPage.data.some((item) => item.id === skill.id), 'R3/E5 Skill list');
  await client.skills.delete(skill.id);

  pass(`registry SDK ${manifest.version} runs Session, Memory, GA Files and GA Skills defaults`);
});

// UserProfiles is Control-owned and intentionally absent from the echo runtime
// topology above. Exercise its existing management behavior owner without
// manufacturing a second route in that topology. Do not pass `betas`: the
// generated SDK method's own version header is the runtime behavior under review
// (0.117.1 emits the legacy selector; 0.120.0 emits the access_type selector).
await withScenarioServer('management', 'mcp', 38191, async (baseURL) => {
  // Cause/effect graph: C3=UserProfiles remains Control-owned and absent from
  // echo; C4=the latest SDK supplies its own default version selector. Effect is
  // a create/retrieve round-trip decoded with access_type + relationship.
  // Decision rule R2: C3 && C4 => use the existing management topology without
  // explicit betas; route/header/shape drift fails rather than adding a canary-only route.
  // Constraints/invariant: Control remains the sole UserProfile owner and the
  // generated SDK remains the sole version-header owner; this canary may not
  // mirror either contract in the echo topology or in hand-authored transport.
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const profile = await client.beta.userProfiles.create({
    access_type: 'application',
    external_id: `latest-sdk-${manifest.version}`,
  });
  assert.equal(profile.access_type, 'application');
  assert.equal(profile.relationship, 'external');
  const response = await client.beta.userProfiles.retrieve(profile.id).withResponse();
  assert.equal(response.data.id, profile.id);
  assert.equal(typeof response.workspace_id, 'string', '0.120 workspace response header');
  pass(`registry SDK ${manifest.version} runs UserProfile default beta`);
});

console.log(`SDK LATEST RUNTIME CANARY PASS: @anthropic-ai/sdk ${manifest.version}.`);
