// The rest of the sessions family, driven by the official Anthropic TypeScript
// SDK (`client.beta.sessions.*`): update / list / delete / archive, the
// `threads` subresource (list / retrieve / archive), and the `resources`
// subresource (add / list / retrieve / update / delete). The create / retrieve /
// events path is covered elsewhere; this exercises the endpoints that were
// missing so any wire-shape drift surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_sessions_family_e2e.mjs

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
    await withScenarioServer('management', 'mcp', 38146, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      const session = await client.beta.sessions.create({ agent: 'assistant', betas: BETAS });
      assert.ok(session.id.startsWith('sesn_'), `id: ${session.id}`);
      assert.equal(session.type, 'session');
      pass('beta.sessions.create -> BetaManagedAgentsSession');

      // -- update + list -----------------------------------------------------
      const updated = await client.beta.sessions.update(session.id, {
        title: 'my session',
        metadata: { team: 'core' },
        betas: BETAS,
      });
      assert.equal(updated.title, 'my session');
      assert.equal(updated.metadata.team, 'core');
      pass('beta.sessions.update -> title + metadata');

      // The update is announced on the event stream as `session.updated`.
      const updEvents = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
      const updatedEv = updEvents.find((e) => e.type === 'session.updated');
      assert.ok(updatedEv, `session.updated projected: ${updEvents.map((e) => e.type)}`);
      assert.equal(updatedEv.title, 'my session');
      assert.equal(updatedEv.metadata.team, 'core');
      pass('session.updated on the event stream');

      const listed = (await drain(client.beta.sessions.list({ betas: BETAS }))).map((s) => s.id);
      assert.ok(listed.includes(session.id));
      pass('beta.sessions.list -> PageCursor<BetaManagedAgentsSession>');

      // -- threads -----------------------------------------------------------
      const threads = await drain(client.beta.sessions.threads.list(session.id, { betas: BETAS }));
      assert.equal(threads.length, 1, 'a fresh session has one primary thread');
      const thread = threads[0];
      assert.equal(thread.type, 'session_thread');
      assert.equal(thread.session_id, session.id);
      const gotThread = await client.beta.sessions.threads.retrieve(thread.id, {
        session_id: session.id,
        betas: BETAS,
      });
      assert.equal(gotThread.id, thread.id);
      pass('beta.sessions.threads.list / retrieve');

      // -- resources ---------------------------------------------------------
      // Upload a real file first so the attach resolves it in the blob store.
      const file = await client.beta.files.upload({
        file: await toFile(Buffer.from('resource contents'), 'in.txt'),
        purpose: 'agent',
        betas: BETAS,
      });
      const res = await client.beta.sessions.resources.add(session.id, {
        type: 'file',
        file_id: file.id,
        mount_path: '/workspace/in.txt',
        betas: BETAS,
      });
      assert.equal(res.type, 'file');
      assert.equal(res.file_id, file.id);
      assert.ok(res.id, 'resource gets an id');
      pass('beta.sessions.resources.add -> BetaManagedAgentsFileResource');

      const resList = await drain(client.beta.sessions.resources.list(session.id, { betas: BETAS }));
      assert.ok(resList.some((r) => r.id === res.id));

      const gotRes = await client.beta.sessions.resources.retrieve(res.id, {
        session_id: session.id,
        betas: BETAS,
      });
      assert.equal(gotRes.id, res.id);

      const upRes = await client.beta.sessions.resources.update(res.id, {
        session_id: session.id,
        mount_path: '/workspace/renamed.txt',
        betas: BETAS,
      });
      assert.equal(upRes.mount_path, '/workspace/renamed.txt');
      pass('beta.sessions.resources.list / retrieve / update');

      const delRes = await client.beta.sessions.resources.delete(res.id, {
        session_id: session.id,
        betas: BETAS,
      });
      assert.equal(delRes.type, 'session_resource_deleted');
      pass('beta.sessions.resources.delete');

      // -- archive + delete --------------------------------------------------
      const archived = await client.beta.sessions.archive(session.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archived session carries archived_at');

      const del = await client.beta.sessions.delete(session.id, { betas: BETAS });
      assert.equal(del.type, 'session_deleted');
      await assert.rejects(
        () => client.beta.sessions.retrieve(session.id, { betas: BETAS }),
        (err) => err.status === 404,
      );
      pass('beta.sessions.archive / delete -> DeletedSession; retrieve 404s after');
    });

    console.log('E2E PASS: the sessions family (update/list/delete/archive + threads + resources) round-trips through @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
