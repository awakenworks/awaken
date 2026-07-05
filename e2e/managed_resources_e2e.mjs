// Real-model Managed Agents RESOURCE e2e (ADR-0038): drive a session through the
// official Anthropic TypeScript SDK against awaken-server-local in `real` mode, and
// prove the resource plane end-to-end with a live model:
//   - Files API: `client.beta.files.upload` stores bytes in the host blob store.
//   - File mount: a session `resources[{type:"file",...}]` is realized into the
//     sandbox; the model reads it (proving mount + read).
//   - Prompt effect: the model only knows WHERE the file is from the system prompt
//     the host injected from the binding (ADR-0038 A3a) — so a correct read proves
//     the prompt reached the model.
//
// Run (from e2e/, with the KIMI key — note the base URL ends in `/v1/`, since the
// executor posts to `{base_url}messages`):
//   ANTHROPIC_API_KEY=sk-kimi-... ANTHROPIC_BASE_URL=https://api.kimi.com/coding/v1/ \
//   ANTHROPIC_MODEL=kimi-k2-0905-preview node managed_resources_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01', 'files-api-2025-04-14'];
const TOKEN = 'ZEBRA_QUASAR_4718'; // a distinctive test marker, not a credential — awaken-allow: secret

async function main() {
  if (!process.env.ANTHROPIC_API_KEY && !process.env.KIMI_API_KEY) {
    console.log('SKIP managed_resources_e2e: no ANTHROPIC_API_KEY / KIMI_API_KEY set.');
    return;
  }
  try {
    await withServer('real', 38137, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // 1. Upload a file (Files API → host blob store).
      const uploaded = await client.beta.files.upload({
        file: await toFile(Buffer.from(`the secret pass phrase is ${TOKEN}`), 'secret.txt'),
        betas: BETAS,
      });
      assert.ok(uploaded.id, 'files.upload returned an id');
      pass(`file uploaded: ${uploaded.id}`);

      // 2. Create a session that mounts the file as a resource.
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        resources: [{ type: 'file', file_id: uploaded.id, mount_path: '/secret.txt' }],
        betas: BETAS,
      });
      assert.equal(session.type, 'session');
      pass(`session created with a file resource: ${session.id}`);

      // 3. Ask the model to read the mounted file. It only learns the path from the
      //    injected system prompt (A3a), so a correct answer proves mount + prompt.
      await client.beta.sessions.events.send(session.id, {
        events: [
          {
            type: 'user.message',
            content: [
              {
                type: 'text',
                text:
                  'A file has been mounted into your sandbox. Read it using your tools ' +
                  'and reply with the exact secret pass phrase it contains, and nothing else.',
              },
            ],
          },
        ],
        betas: BETAS,
      });

      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
        events.push(ev);
      }
      const types = events.map((e) => e.type);
      assert.ok(types.includes('session.status_idle'), `expected status_idle, got ${types}`);
      // The model must have actually read the mounted file via its tools.
      const readCall = events.find(
        (e) => e.type === 'agent.tool_use' && e.name === 'read',
      );
      assert.ok(readCall, `expected the model to call the read tool; got ${types}`);
      pass(`model called read(${readCall.input?.path}) on the mounted resource`);

      const said = events
        .filter((e) => e.type === 'agent.message')
        .flatMap((m) => (m.content ?? []).map((c) => c.text ?? ''))
        .join(' ');
      assert.ok(
        said.includes(TOKEN),
        `model must reproduce the mounted file's secret token; got: ${JSON.stringify(said.slice(0, 200))}`,
      );
      pass(`model read the mounted file and reproduced the token: ${TOKEN}`);

      // 4. ARTIFACT write + retrieval: the model writes a file under `outputs/`; the
      //    host harvests it, and we retrieve it via files.list(scope_id) + download.
      const ARTIFACT = 'DONE_9931'; // awaken-allow: secret
      await client.beta.sessions.events.send(session.id, {
        events: [
          {
            type: 'user.message',
            content: [
              {
                type: 'text',
                text:
                  `Using your tools, write the exact text ${ARTIFACT} into a new file ` +
                  `at the path outputs/result.txt. Reply with "written" when done.`,
              },
            ],
          },
        ],
        betas: BETAS,
      });
      // `write` is not auto-allowed (only read/glob/grep are), so the run parks for a
      // confirmation. Approve each pending write tool_use, then wait for it to land.
      const approved = new Set();
      let files = [];
      for (let attempt = 0; attempt < 20 && files.length === 0; attempt++) {
        await new Promise((r) => setTimeout(r, 1500));
        const evs = [];
        for await (const e of client.beta.sessions.events.list(session.id, { betas: BETAS })) evs.push(e);
        for (const e of evs) {
          if (
            e.type === 'agent.tool_use' &&
            e.evaluated_permission === 'ask' &&
            !approved.has(e.id)
          ) {
            approved.add(e.id);
            await client.beta.sessions.events.send(session.id, {
              events: [{ type: 'user.tool_confirmation', tool_use_id: e.id, result: 'allow' }],
              betas: BETAS,
            });
          }
        }
        files = [];
        for await (const f of client.beta.files.list({ scope_id: session.id, betas: BETAS })) {
          files.push(f);
        }
      }
      assert.ok(approved.size > 0, 'the write tool should have parked for a confirmation');
      pass(`approved ${approved.size} gated tool call(s) via user.tool_confirmation`);
      assert.ok(files.length > 0, `session artifacts should be listed; got ${files.length}`);
      const artifact = files.find((f) => (f.filename ?? '').includes('result.txt')) ?? files[0];
      pass(`artifact listed via files.list(scope_id): ${artifact.filename} (${artifact.id.slice(0, 12)})`);
      const resp = await client.beta.files.download(artifact.id, { betas: BETAS });
      const text = await resp.text();
      assert.ok(
        text.includes(ARTIFACT),
        `downloaded artifact must contain ${ARTIFACT}; got ${JSON.stringify(text.slice(0, 120))}`,
      );
      pass(`artifact written by the model, harvested + downloaded by the host: ${ARTIFACT}`);
    });

    console.log('E2E PASS: file resource read + artifact write/retrieve verified end-to-end with a real model via the official SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
