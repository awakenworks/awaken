// Files + Models, driven through both official Anthropic TypeScript SDK roots:
// `client.beta.{files,models}` and GA `client.{files,models}`. Both roots call the
// same catalog/store owners; this scenario verifies their distinct wire projections
// without creating a second resource implementation.
//
// Run: (from e2e/)  node management_files_models_e2e.mjs
//
// Causal graph: C1=Beta or GA SDK root; C2=durable File identity exists/missing;
// C3=published executable Model exists/missing. Effects: E1=each root decodes its
// exact metadata/list/retrieve response; E2=input download is denied by the shared
// file policy; E3=delete makes later metadata fail; E4=missing Model returns 404.
// Downloadable artifact success is owned by the Session namespace E2E. Constraints:
// both roots share one FileCatalog/model inventory and never copy state.
// Decision table:
// | rule | root | identity | operation | effects |
// | R1 | Beta | existing File | delete | E1+E3 |
// | R2 | GA | existing input File | upload/list/retrieve/download/delete | E1+E2+E3 |
// | R3 | Beta/GA | published Model | list/retrieve | E1 |
// | R4 | Beta/GA | missing Model | retrieve | E4 |

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { FAKE_KEY, withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const gaFilesMode = process.env.AWAKEN_MANAGED_SDK_HAS_GA_FILES ?? '1';
assert.match(gaFilesMode, /^(?:0|1)$/u, 'GA Files capability mode must be 0 or 1');
const HAS_GA_FILES = gaFilesMode === '1';

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function main() {
  try {
    await withScenarioServer('management-providers', 'mcp', 38134, async (baseUrl, upstream) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // -- Files: upload → delete → 404 -------------------------------------
      const up = await client.beta.files.upload({
        file: await toFile(Buffer.from('to-be-deleted'), 'gone.txt'),
      });
      assert.ok(up.id, 'upload returns a content id');
      const deleted = await client.beta.files.delete(up.id);
      assert.equal(deleted.type, 'file_deleted');
      assert.equal(deleted.id, up.id);
      await assert.rejects(
        () => client.beta.files.retrieveMetadata(up.id),
        (err) => err.status === 404,
      );
      pass('beta.files.delete -> DeletedFile; metadata 404s after');

      // GA Files uses the same durable File authority but a different metadata
      // projection and no beta selector. Exercise all five current SDK methods.
      if (HAS_GA_FILES) {
        const gaFile = await client.files.upload({
          file: await toFile(Buffer.from('ga-file-content'), 'ga-file.txt'),
          expires_in_seconds: 3600,
        });
        assert.equal(gaFile.type, 'file', 'R2/E1');
        assert.equal(typeof gaFile.expires_at, 'string', 'R2 GA expiry projection');
        assert.equal((await client.files.retrieveMetadata(gaFile.id)).id, gaFile.id, 'R2/E1');
        const gaFiles = await drain(client.files.list({ ids: [gaFile.id, 'file_missing'] }));
        assert.deepEqual(gaFiles.map((file) => file.id), [gaFile.id], 'R2/E1 ids filter');
        await assert.rejects(
          () => client.files.download(gaFile.id),
          (error) => error?.status === 400 && String(error).includes('not downloadable'),
          'R2/E2 GA input download fails closed',
        );
        assert.equal((await client.files.delete(gaFile.id)).type, 'file_deleted', 'R2/E3');
        await assert.rejects(
          () => client.files.retrieveMetadata(gaFile.id),
          (error) => error?.status === 404,
        );
        pass('GA Files upload/list/retrieve/download/delete share the canonical File authority');
      }

      // -- Models: list + retrieve + alias-miss 404 -------------------------
      // Model-directory cause/effect rules: M1 no authored catalog facts ->
      // the production live directory is empty (never fixture defaults); M2 an
      // explicit Provider Connection discovers the fake provider's exact model;
      // M3 an Agent publication freezes that catalog route into the Coordinator's
      // executable inventory -> `/v1/models` projects it. The shared fake provider
      // owns inference and discovery, avoiding a second model-server implementation.
      const connected = await fetch(`${baseUrl}/v1/config/provider-connections`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          idempotency_key: `files-models-${process.pid}`,
          workspace_id: 'default',
          provider_id: 'anthropic',
          display_name: 'Files Models E2E',
          dialect: 'anthropic_messages',
          base_url: `${upstream.url}/v1/`,
          timeout_secs: 30,
          secret: FAKE_KEY,
        }),
      });
      assert.equal(connected.status, 201, await connected.text());
      const authored = await fetch(`${baseUrl}/v1/config/agents/files-models-agent`, {
        method: 'PUT',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          id: 'files-models-agent',
          name: 'Files Models E2E',
          instructions: 'Exercise the executable model directory.',
          model: {
            mode: 'pinned',
            provider_identity_ref: 'anthropic',
            model_ref: 'claude-opus-4-8',
            backend_ref: 'genai',
          },
          tools: [],
        }),
      });
      assert.equal(authored.status, 200, await authored.text());
      const published = await fetch(
        `${baseUrl}/v1/config/agents/files-models-agent/publish`,
        { method: 'POST' },
      );
      assert.equal(published.status, 200, await published.text());
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

      const gaModels = await drain(client.models.list());
      assert.ok(gaModels.some((model) => model.id === 'claude-opus-4-8'), 'R3 GA model list');
      assert.ok(
        gaModels.every((model) => model.type === 'model' && typeof model.display_name === 'string'),
        'R3/E1 every GA entry decodes as ModelInfo',
      );
      assert.ok(
        gaModels.every((model) => !Object.hasOwn(model, 'allowed_fallback_models')),
        'R3 GA ModelInfo omits the Beta-only fallback field',
      );
      const gaModel = await client.models.retrieve('claude-opus-4-8');
      assert.equal(gaModel.id, 'claude-opus-4-8', 'R3 GA retrieve');
      assert.equal(gaModel.type, 'model');
      assert.equal(Object.hasOwn(gaModel, 'allowed_fallback_models'), false, 'R3 GA projection');
      pass('GA models.list/retrieve -> ModelInfo over the same executable inventory');

      await assert.rejects(
        () => client.beta.models.retrieve('no-such-model', { betas: BETAS }),
        (err) => err.status === 404,
      );
      await assert.rejects(
        () => client.models.retrieve('no-such-model'),
        (err) => err.status === 404,
      );
      pass('Beta and GA models.retrieve(unknown) -> 404');
    }, {}, { upstream: { models: ['claude-opus-4-8'] } });

    console.log('E2E PASS: files.delete + the Models API round-trip through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
