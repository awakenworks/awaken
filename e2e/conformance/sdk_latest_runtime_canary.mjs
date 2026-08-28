// Runtime half of the registry-latest SDK canary. The declaration fingerprint
// detects shape drift; this executable smoke detects generated path, default
// beta, pagination and response-decoding drift against the real Awaken
// topologies that own each resource family.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { extractOperationsFromPackageRoot } from '../../packages/managed-sdk-oracle/src/extract-operations.mjs';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import {
  pass,
  waitForSessionEventReceipt,
  withRealServer,
  withScenarioServer,
} from '../harness.mjs';
import { officialBetaResourceProjection } from './official_sdk_resource_projection.mjs';
import {
  assertLatestRuntimeOwnsCandidateDelta,
  officialSdkCandidateDelta,
} from './official_sdk_candidate_delta.mjs';
import { exerciseOfficialWebhookContract } from './official_webhook_contract.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '../..');
const packageRoot = process.env.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
assert.ok(packageRoot, 'ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT is required');
const manifest = JSON.parse(readFileSync(resolve(packageRoot, 'package.json'), 'utf8'));
const { default: Anthropic, toFile } = await import(pathToFileURL(resolve(packageRoot, 'index.mjs')));
const scope = JSON.parse(readFileSync(
  resolve(REPO, 'packages/managed-sdk-oracle/config/scope.json'),
  'utf8',
));
const oracle = JSON.parse(readFileSync(
  resolve(REPO, 'contracts/anthropic-managed/upstream-oracle.generated.json'),
  'utf8',
));
const candidateDelta = officialSdkCandidateDelta(
  resolveSdkPackage(oracle.current.module).root,
  packageRoot,
  scope,
);
assertLatestRuntimeOwnsCandidateDelta(candidateDelta);
const { operations } = extractOperationsFromPackageRoot(packageRoot, scope);
const betaFiles = officialBetaResourceProjection(operations, 'files');
const betaSkills = officialBetaResourceProjection(operations, 'skills');
const webhookProfile = exerciseOfficialWebhookContract(Anthropic);

async function drain(pagePromise) {
  const rows = [];
  for await (const row of pagePromise) rows.push(row);
  return rows;
}

async function exerciseBetaFiles(client) {
  // Cause/effect graph: C6 the official generated Beta Files operations carry
  // their historical capability header; C7 they retain beta=true but carry no
  // capability after GA. Effects: E6 decode Beta metadata/Page; E7 decode GA
  // metadata/PageCursor while staying under client.beta.files; E8 every method
  // reaches the one File authority. Decision rules F1 C6->E6+E8;
  // F2 C7->E7+E8. The generated operation inventory, not an SDK version branch,
  // selects the request and assertions.
  const file = await client.beta.files.upload({
    file: await toFile(Buffer.from(manifest.version), 'latest-beta.txt'),
    ...(betaFiles.projection === 'ga' ? { expires_in_seconds: 3_600 } : {}),
  });
  assert.equal(file.type, 'file', 'F1/F2 shared File identity');
  assert.equal(file.filename, 'latest-beta.txt', 'F1/F2 metadata');
  if (betaFiles.projection === 'beta') {
    assert.equal(Object.hasOwn(file, 'expires_at'), false, 'F1/E6');
  } else {
    assert.equal(typeof file.expires_at, 'string', 'F2/E7');
    assert.equal(Object.hasOwn(file, 'scope'), false, 'F2/E7');
  }
  assert.equal((await client.beta.files.retrieveMetadata(file.id)).id, file.id, 'F1/F2 retrieve');
  const listed = await drain(client.beta.files.list(
    betaFiles.projection === 'ga' ? { ids: [file.id, 'file_missing'] } : {},
  ));
  assert.ok(listed.some(({ id }) => id === file.id), 'F1/F2 list');
  if (betaFiles.projection === 'ga') {
    assert.deepEqual(listed.map(({ id }) => id), [file.id], 'F2/E7 ids[]');
  }
  await assert.rejects(
    () => client.beta.files.download(file.id),
    (error) => error?.status === 400 && String(error).includes('not downloadable'),
    'F1/F2 input download policy',
  );
  assert.equal((await client.beta.files.delete(file.id)).type, 'file_deleted', 'F1/F2 delete');
}

async function exerciseBetaSkills(client) {
  // Cause/effect graph: S1 generated Beta Skills operations carry the Skills
  // capability; S2 post-GA operations keep beta=true without it. Effects:
  // E1 historical display_title/latest_version/version projection; E2 GA
  // display_name/latest_version_id/id projection; E3 all nine generated methods,
  // including archive download, share one SkillStore lifecycle. Decision rules:
  // S1->E1+E3; S2->E2+E3. The operation signature is the only discriminator.
  const document = '---\nname: latest-beta-canary\ndescription: first\n---\nFirst.';
  const revised = '---\nname: latest-beta-canary\ndescription: second\n---\nSecond.';
  const skill = await client.beta.skills.create({
    ...(betaSkills.projection === 'beta'
      ? { display_title: 'Latest Beta Canary' }
      : { display_name: 'Latest Beta Canary' }),
    files: [await toFile(Buffer.from(document), 'SKILL.md')],
  });
  assert.equal(skill.type, 'skill', 'S1/S2 create');
  const firstVersion = betaSkills.projection === 'beta'
    ? skill.latest_version
    : skill.latest_version_id;
  assert.equal(typeof firstVersion, 'string', 'S1/S2 initial version');
  if (betaSkills.projection === 'beta') {
    assert.equal(skill.display_title, 'Latest Beta Canary', 'S1/E1');
    assert.equal(Object.hasOwn(skill, 'display_name'), false, 'S1/E1');
  } else {
    assert.equal(skill.display_name, 'Latest Beta Canary', 'S2/E2');
    assert.equal(skill.source.type, 'custom', 'S2/E2');
    assert.equal(Object.hasOwn(skill, 'display_title'), false, 'S2/E2');
  }
  assert.equal((await client.beta.skills.retrieve(skill.id)).id, skill.id, 'S1/S2 retrieve');
  assert.ok(
    (await drain(client.beta.skills.list())).some(({ id }) => id === skill.id),
    'S1/S2 list',
  );

  const version = await client.beta.skills.versions.create(skill.id, {
    files: [await toFile(Buffer.from(revised), 'SKILL.md')],
  });
  const versionReference = betaSkills.projection === 'beta' ? version.version : version.id;
  assert.equal(typeof versionReference, 'string', 'S1/S2 version create');
  assert.equal(
    (await client.beta.skills.versions.retrieve(versionReference, { skill_id: skill.id })).skill_id,
    skill.id,
    'S1/S2 version retrieve',
  );
  const versions = await drain(client.beta.skills.versions.list(skill.id));
  assert.ok(versions.some((item) => (
    betaSkills.projection === 'beta' ? item.version : item.id
  ) === versionReference), 'S1/S2 version list');
  const archive = await client.beta.skills.versions.download(versionReference, {
    skill_id: skill.id,
  });
  assert.match(await archive.text(), /Second\./u, 'S1/S2 archive download');
  assert.equal(
    (await client.beta.skills.versions.delete(firstVersion, { skill_id: skill.id })).type,
    'skill_version_deleted',
    'S1/S2 version delete',
  );
  assert.equal((await client.beta.skills.delete(skill.id)).type, 'skill_deleted', 'S1/S2 delete');
}

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

  await exerciseBetaFiles(client);
  await exerciseBetaSkills(client);

  // Cause/effect graph: C3=0.120 exposes GA Files and Skills outside `beta`;
  // C4=both project the existing FileCatalog/SkillStore; C5=GA Files expiry and
  // ids[] pagination, GA Model capabilities, and GA Skill source/latest-version
  // fields differ from beta.
  // Effects: E3 generated GA paths run without a beta header, E4 exact GA DTOs
  // decode, E5 the same created ids remain visible through their one repository.
  // Decision table: R3 C3+C4+C5 -> every GA Files/Skills method round-trips;
  // R4 C3+C4 -> GA Models list/retrieve decode from the existing inventory;
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
  await assert.rejects(
    () => client.files.download(file.id),
    (error) => error?.status === 400 && String(error).includes('not downloadable'),
    'R3 uploaded inputs remain non-downloadable through the latest GA client',
  );
  await client.files.delete(file.id);

  const models = [];
  for await (const model of client.models.list()) models.push(model);
  assert.ok(models.length > 0, 'R4 GA Models list is non-empty');
  assert.ok(models.every((model) => !Object.hasOwn(model, 'allowed_fallback_models')), 'R4 GA shape');
  assert.equal((await client.models.retrieve(models[0].id)).id, models[0].id, 'R4 retrieve');

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
  const version = await client.skills.versions.create(skill.id, {
    files: [await toFile(
      Buffer.from(`---\nname: latest-skill\ndescription: SDK ${manifest.version} v2\n---\n`),
      'latest-skill/SKILL.md',
    )],
  });
  assert.equal(
    (await client.skills.versions.retrieve(version.id, { skill_id: skill.id })).id,
    version.id,
  );
  const versionPage = await client.skills.versions.list(skill.id);
  assert.ok(versionPage.data.some((item) => item.id === version.id), 'R3 Skill Version list');
  assert.equal(
    (await client.skills.versions.delete(skill.latest_version_id, { skill_id: skill.id })).type,
    'skill_version_deleted',
  );
  await client.skills.delete(skill.id);

  pass(
    `registry SDK ${manifest.version} runs Session, Memory, Beta/GA Models, Files, and Skills defaults`,
  );
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

console.log(
  `SDK LATEST RUNTIME CANARY PASS: @anthropic-ai/sdk ${manifest.version}; `
  + `beta.files=${betaFiles.projection}, beta.skills=${betaSkills.projection}, `
  + `parseUnverified=${webhookProfile.parseUnverified}, `
  + `operation_changes=${candidateDelta.operations.changed.length}, `
  + `declaration_changes=${candidateDelta.declarations.changed.length}.`,
);
