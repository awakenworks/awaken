// Files.delete + the Models API, driven by the official Anthropic TypeScript SDK
// (`client.beta.files.*`, `client.beta.models.*`). The upload/download/idempotency
// path is covered by `managed_resources_api_e2e.mjs`; this file adds the previously
// missing `files.delete` and the whole Models API so any wire-shape drift from the
// official `DeletedFile` / `BetaModelInfo` types surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_files_models_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38134, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // -- Files: upload → delete → 404 -------------------------------------
      const up = await client.beta.files.upload({
        file: await toFile(Buffer.from('to-be-deleted'), 'gone.txt'),
        purpose: 'agent',
        betas: BETAS,
      });
      assert.ok(up.id, 'upload returns a content id');
      const deleted = await client.beta.files.delete(up.id, { betas: BETAS });
      assert.equal(deleted.type, 'file_deleted');
      assert.equal(deleted.id, up.id);
      await assert.rejects(
        () => client.beta.files.retrieveMetadata(up.id, { betas: BETAS }),
        (err) => err.status === 404,
      );
      pass('beta.files.delete -> DeletedFile; metadata 404s after');

      // -- Models: list + retrieve + alias-miss 404 -------------------------
      const models = await drain(client.beta.models.list({ betas: BETAS }));
      assert.ok(models.length >= 1, 'models list is non-empty');
      assert.ok(
        models.every((m) => m.type === 'model' && typeof m.display_name === 'string'),
        'every entry decodes as BetaModelInfo',
      );
      assert.ok(models.some((m) => m.id === 'claude-opus-4-8'), 'the configured model is listed');
      pass(`beta.models.list -> Page<BetaModelInfo> (${models.length} models)`);

      const one = await client.beta.models.retrieve('claude-opus-4-8', { betas: BETAS });
      assert.equal(one.id, 'claude-opus-4-8');
      assert.equal(one.type, 'model');
      pass('beta.models.retrieve -> BetaModelInfo');

      await assert.rejects(
        () => client.beta.models.retrieve('no-such-model', { betas: BETAS }),
        (err) => err.status === 404,
      );
      pass('beta.models.retrieve(unknown) -> 404');
    });

    console.log('E2E PASS: files.delete + the Models API round-trip through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
