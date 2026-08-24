// The skills family, driven by the official Anthropic TypeScript SDK
// (`client.beta.skills.*` + `client.beta.skills.versions.*`): multipart create,
// retrieve, list, delete, and the version subresource (create / retrieve / list /
// delete / download). Any wire-shape drift from the official `SkillCreateResponse`
// / `VersionCreateResponse` types surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_skills_e2e.mjs
//
// Causal graph: validated multipart SKILL.md -> immutable version -> download /
// new version / delete -> latest-version and archive state update atomically.
// Decision table:
// | bundle | identity/version | operation | observable behavior |
// | valid | new | create | version 1 becomes latest |
// | malformed/duplicate | any | create | 400/409 and no partial Skill |
// | valid | next | version create/delete | latest pointer follows durable versions |

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = [];
const SKILLS_BETA = 'skills-2025-10-02';
const SKILL_MD_V1 = '---\nname: greeter\ndescription: says hi\n---\nSay hi to the user.';
const SKILL_MD_V2 = '---\nname: greeter\ndescription: says hi (v2)\n---\nSay a warm hi.';
const GA_SKILL_MD_V1 = '---\nname: ga-greeter\ndescription: says hi through GA\n---\nSay hi.';
const GA_SKILL_MD_V2 = '---\nname: ga-greeter\ndescription: says hi through GA (v2)\n---\nSay hi warmly.';

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function expectStatus(action, status) {
  await assert.rejects(action, (error) => error.status === status);
}

async function uploadRaw(baseUrl, files) {
  const form = new FormData();
  for (const [name, bytes] of files) {
    form.append('files[]', new Blob([bytes]), name);
  }
  return fetch(`${baseUrl}/v1/skills`, {
    method: 'POST',
    headers: { 'anthropic-beta': SKILLS_BETA },
    body: form,
  });
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38142, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Causes: Skills collection request has no selector, only an unrelated
      // Managed beta, or `beta=true` without the endpoint-specific Skills beta.
      // Effects: the first two select GA and succeed; the incomplete explicit
      // beta selector rejects before mutation. Constraint: unrelated betas do
      // not replace the Skills selector. Decision rules A7.1 !selector->GA;
      // A7.2 unrelated header->GA; A7.3 beta=true&&!SkillsHeader->400.
      for (const beta of [null, 'managed-agents-2026-04-01']) {
        const accepted = await fetch(`${baseUrl}/v1/skills`, {
          headers: beta ? { 'anthropic-beta': beta } : {},
        });
        assert.equal(accepted.status, 200);
      }
      assert.equal((await fetch(`${baseUrl}/v1/skills?beta=true`)).status, 400);

      // Create a skill via a multipart SKILL.md upload.
      const skill = await client.beta.skills.create({
        display_title: 'Greeter',
        files: [await toFile(Buffer.from(SKILL_MD_V1), 'SKILL.md')],
        betas: BETAS,
      });
      assert.equal(skill.type, 'skill');
      assert.ok(skill.id.startsWith('skill_'), `id: ${skill.id}`);
      assert.equal(skill.display_title, 'Greeter');
      assert.equal(skill.latest_version, '1');
      pass('beta.skills.create -> SkillCreateResponse (multipart)');

      await expectStatus(
        async () =>
          client.beta.skills.create({
            files: [await toFile(Buffer.from(SKILL_MD_V1), 'SKILL.md')],
            betas: BETAS,
          }),
        409,
      );
      pass('duplicate durable Skill identity -> 409');

      assert.equal((await uploadRaw(baseUrl, [['notes.txt', 'not a skill']])).status, 400);
      assert.equal((await uploadRaw(baseUrl, [['SKILL.md', Buffer.from([0xff, 0xfe])]])).status, 400);
      assert.equal(
        (
          await uploadRaw(baseUrl, [
            ['SKILL.md', SKILL_MD_V1],
            ['SKILL.md', SKILL_MD_V2],
          ])
        ).status,
        400,
      );
      pass('missing, non-UTF8, and duplicate SKILL.md bundles fail closed');

      const got = await client.beta.skills.retrieve(skill.id, { betas: BETAS });
      assert.equal(got.id, skill.id);
      pass('beta.skills.retrieve');

      const skillIds = (await drain(client.beta.skills.list({ betas: BETAS }))).map((s) => s.id);
      assert.ok(skillIds.includes(skill.id));
      pass('beta.skills.list -> PageCursor<SkillListResponse>');

      await expectStatus(() => client.beta.skills.retrieve('skill_missing', { betas: BETAS }), 404);
      await expectStatus(
        async () =>
          client.beta.skills.versions.create('skill_missing', {
            files: [await toFile(Buffer.from(SKILL_MD_V2), 'SKILL.md')],
            betas: BETAS,
          }),
        404,
      );
      await expectStatus(
        () => drain(client.beta.skills.versions.list('skill_missing', { betas: BETAS })),
        404,
      );
      pass('unknown Skill and its version collection -> 404');

      // Add a second version.
      const v2 = await client.beta.skills.versions.create(skill.id, {
        files: [await toFile(Buffer.from(SKILL_MD_V2), 'SKILL.md')],
        betas: BETAS,
      });
      assert.equal(v2.type, 'skill_version');
      assert.equal(v2.skill_id, skill.id);
      assert.equal(v2.version, '2');
      assert.equal(v2.name, 'greeter');
      pass('beta.skills.versions.create -> VersionCreateResponse');

      const versions = (await drain(client.beta.skills.versions.list(skill.id, { betas: BETAS }))).map(
        (v) => v.version,
      );
      assert.deepEqual(versions, ['1', '2']);
      pass('beta.skills.versions.list');

      const v1 = await client.beta.skills.versions.retrieve('1', { skill_id: skill.id, betas: BETAS });
      assert.equal(v1.version, '1');
      assert.equal(v1.description, 'says hi');
      pass('beta.skills.versions.retrieve');

      const latest = await client.beta.skills.versions.retrieve('latest', {
        skill_id: skill.id,
        betas: BETAS,
      });
      assert.equal(latest.version, '2');
      const byId = await client.beta.skills.versions.retrieve(v2.id, {
        skill_id: skill.id,
        betas: BETAS,
      });
      assert.equal(byId.version, '2');
      await expectStatus(
        () => client.beta.skills.versions.retrieve('404', { skill_id: skill.id, betas: BETAS }),
        404,
      );
      pass('latest, immutable version id, and missing version references are distinct');

      const download = await client.beta.skills.versions.download('2', { skill_id: skill.id, betas: BETAS });
      assert.equal(download.headers.get('content-type'), 'application/x-tar');
      const body = await download.text();
      assert.ok(body.includes('worker-greeter') || body.includes('greeter'));
      assert.ok(body.includes('Say a warm hi'), 'archive contains the immutable version content');
      pass('beta.skills.versions.download returns the archive consumed by setupSkills');

      const missingFile = await fetch(
        `${baseUrl}/v1/skills/${skill.id}/versions/2/files/references/missing.md`,
        { headers: { 'anthropic-beta': SKILLS_BETA } },
      );
      assert.equal(missingFile.status, 404);
      const missingVersionFile = await fetch(
        `${baseUrl}/v1/skills/${skill.id}/versions/404/files/references/missing.md`,
        { headers: { 'anthropic-beta': SKILLS_BETA } },
      );
      assert.equal(missingVersionFile.status, 404);
      await expectStatus(
        () => client.beta.skills.versions.download('404', { skill_id: skill.id, betas: BETAS }),
        404,
      );
      pass('unknown bundle file/content/version -> 404');

      const delVer = await client.beta.skills.versions.delete('1', { skill_id: skill.id, betas: BETAS });
      assert.equal(delVer.type, 'skill_version_deleted');
      pass('beta.skills.versions.delete');

      await expectStatus(
        () => client.beta.skills.versions.delete('1', { skill_id: skill.id, betas: BETAS }),
        404,
      );
      await expectStatus(
        () => client.beta.skills.versions.delete('2', { skill_id: skill.id, betas: BETAS }),
        400,
      );
      pass('retired version stays absent and the sole live version cannot be deleted');

      const delSkill = await client.beta.skills.delete(skill.id, { betas: BETAS });
      assert.equal(delSkill.type, 'skill_deleted');
      await expectStatus(() => client.beta.skills.retrieve(skill.id, { betas: BETAS }), 404);
      await expectStatus(() => client.beta.skills.delete(skill.id, { betas: BETAS }), 404);
      await expectStatus(
        () => drain(client.beta.skills.versions.list(skill.id, { betas: BETAS })),
        404,
      );
      pass('beta.skills.delete -> SkillDeleteResponse; repeated reads/deletes 404');

      // GA Skills cause/effect graph: C4=GA root omits every beta selector;
      // C5=one valid bundle creates version 1; C6=a second valid bundle advances
      // latest while version 1 remains addressable. Effects: E4=all four Skill
      // and all four GA Version SDK methods decode; E5=deleting version 1 leaves
      // version 2 authoritative; E6=deleting the Skill removes the aggregate.
      // Constraints: GA and Beta project the same SkillStore; GA has no archive
      // download method. Decision rules: R4 C4+C5 -> create/retrieve/list;
      // R5 C4+C5+C6 -> version create/retrieve/list/delete + E5;
      // R6 C4+E5 -> Skill delete + E6.
      const gaSkill = await client.skills.create({
        display_name: 'GA Greeter',
        files: [await toFile(Buffer.from(GA_SKILL_MD_V1), 'ga-greeter/SKILL.md')],
      });
      assert.equal(gaSkill.type, 'skill', 'R4/E4');
      assert.equal(gaSkill.display_name, 'GA Greeter');
      assert.equal(gaSkill.source.type, 'custom');
      assert.equal((await client.skills.retrieve(gaSkill.id)).id, gaSkill.id, 'R4/E4');
      assert.ok(
        (await drain(client.skills.list({ source: 'custom' }))).some((item) => item.id === gaSkill.id),
        'R4/E4',
      );

      const gaV2 = await client.skills.versions.create(gaSkill.id, {
        files: [await toFile(Buffer.from(GA_SKILL_MD_V2), 'ga-greeter/SKILL.md')],
      });
      assert.equal(gaV2.type, 'skill_version', 'R5/E4');
      assert.equal(gaV2.skill_id, gaSkill.id);
      assert.equal(
        (await client.skills.versions.retrieve(gaV2.id, { skill_id: gaSkill.id })).id,
        gaV2.id,
        'R5/E4',
      );
      const gaVersions = await drain(client.skills.versions.list(gaSkill.id));
      assert.deepEqual(gaVersions.map((version) => version.id), [gaSkill.latest_version_id, gaV2.id]);
      assert.equal(
        (await client.skills.versions.delete(gaSkill.latest_version_id, { skill_id: gaSkill.id })).type,
        'skill_version_deleted',
        'R5/E5',
      );
      assert.equal((await client.skills.delete(gaSkill.id)).type, 'skill_deleted', 'R6/E6');
      await expectStatus(() => client.skills.retrieve(gaSkill.id), 404);
      pass('GA Skills and Versions methods share one durable SkillStore with Beta');
    });

    console.log('E2E PASS: the skills family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
