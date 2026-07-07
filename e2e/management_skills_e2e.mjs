// The skills family, driven by the official Anthropic TypeScript SDK
// (`client.beta.skills.*` + `client.beta.skills.versions.*`): multipart create,
// retrieve, list, delete, and the version subresource (create / retrieve / list /
// delete / download). Any wire-shape drift from the official `SkillCreateResponse`
// / `VersionCreateResponse` types surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_skills_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const SKILL_MD_V1 = '---\nname: greeter\ndescription: says hi\n---\nSay hi to the user.';
const SKILL_MD_V2 = '---\nname: greeter\ndescription: says hi (v2)\n---\nSay a warm hi.';

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withServer('management', 38142, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

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

      const got = await client.beta.skills.retrieve(skill.id, { betas: BETAS });
      assert.equal(got.id, skill.id);
      pass('beta.skills.retrieve');

      const skillIds = (await drain(client.beta.skills.list({ betas: BETAS }))).map((s) => s.id);
      assert.ok(skillIds.includes(skill.id));
      pass('beta.skills.list -> PageCursor<SkillListResponse>');

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

      const download = await client.beta.skills.versions.download('2', { skill_id: skill.id, betas: BETAS });
      const body = await download.text();
      assert.ok(body.includes('Say a warm hi'), 'download returns the version content');
      pass('beta.skills.versions.download');

      const delVer = await client.beta.skills.versions.delete('1', { skill_id: skill.id, betas: BETAS });
      assert.equal(delVer.type, 'skill_version_deleted');
      pass('beta.skills.versions.delete');

      const delSkill = await client.beta.skills.delete(skill.id, { betas: BETAS });
      assert.equal(delSkill.type, 'skill_deleted');
      await assert.rejects(
        () => client.beta.skills.retrieve(skill.id, { betas: BETAS }),
        (err) => err.status === 404,
      );
      pass('beta.skills.delete -> SkillDeleteResponse; retrieve 404s after');
    });

    console.log('E2E PASS: the skills family round-trips through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
