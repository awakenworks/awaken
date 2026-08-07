// The rest of the sessions family, driven by the official Anthropic TypeScript
// SDK (`client.beta.sessions.*`): update / list / delete / archive, the
// `threads` subresource (list / retrieve / archive), and the `resources`
// subresource (add / list / retrieve / update / delete). The create / retrieve /
// events path is covered elsewhere; this exercises the endpoints that were
// missing so any wire-shape drift surfaces as an SDK decode error.
//
// Run: (from e2e/)  node management_sessions_family_e2e.mjs
//
// Root-update cause graph:
//   C1 Session absent -> U1 404, no effect
//   C2 fields equal current values + no command key -> U2 semantic no-op
//   C3 metadata key carries null -> U3 delete key + one update event
//   C4 malformed update body or command precondition -> U4 reject before mutation
//   C5 wildcard precondition -> U5 apply against the current root revision
//   C6 metadata bag is null -> U6 clear the complete bag
//
// Decision table:
// | Rule | Session | semantic delta | key | Effect |
// |---|---|---|---|---|
// | U1 | absent | any | none | 404 |
// | U2 | present | none | none | same ETag, no event |
// | U3 | present | delete metadata | none | new ETag, key absent, one event |
// | U4 | present | malformed agent/header | invalid | 400, unchanged root |
// | U5 | present | title change | If-Match: * | apply, new title |
// | U6 | present | metadata null | none | clear all metadata keys |
//
// Terminal-lifecycle cause graph: C7 archive and delete are distinct terminal
// commands; C8 a terminal aggregate cannot transition to another terminal
// state. Effects: U7 archive retains a terminated tombstone; U8 delete removes
// a live aggregate; U9 archive-then-delete is rejected without rewriting the
// terminal fact. The family test therefore uses two independent live Sessions
// instead of encoding the invalid `terminated -> deleted` transition as success.
//
// | Rule | Initial state | Command | Effect |
// |---|---|---|---|
// | U7 | live | archive | terminated tombstone with `archived_at` |
// | U8 | live | delete | `session_deleted`; retrieve returns 404 |
// | U9 | terminated | delete | 404; archived tombstone remains authoritative |

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

async function drain(pagePromise) {
  const items = [];
  for await (const item of pagePromise) items.push(item);
  return items;
}

async function rawUpdate(baseUrl, sessionId, body, headers = {}) {
  return fetch(`${baseUrl}/v1/sessions/${sessionId}`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'anthropic-beta': BETAS[0],
      ...headers,
    },
    body: JSON.stringify(body),
  });
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

      await assert.rejects(
        () => client.beta.sessions.update('sesn_missing', { title: 'never', betas: BETAS }),
        (err) => err.status === 404,
        'U1 an absent aggregate is rejected',
      );

      const beforeNoop = await client.beta.sessions
        .retrieve(session.id, { betas: BETAS })
        .withResponse();
      const beforeNoopEtag = beforeNoop.response.headers.get('etag');
      assert.ok(beforeNoopEtag, 'retrieve exposes the root revision');
      const beforeNoopEvents = (await drain(
        client.beta.sessions.events.list(session.id, { betas: BETAS }),
      )).filter((event) => event.type === 'session.updated').length;
      const noOp = await client.beta.sessions
        .update(session.id, { title: 'my session', metadata: { team: 'core' }, betas: BETAS })
        .withResponse();
      assert.equal(noOp.response.headers.get('etag'), beforeNoopEtag, 'U2 preserves revision');
      assert.equal(
        (await drain(client.beta.sessions.events.list(session.id, { betas: BETAS })))
          .filter((event) => event.type === 'session.updated').length,
        beforeNoopEvents,
        'U2 emits no domain event',
      );

      const removedMetadata = await client.beta.sessions
        .update(session.id, { metadata: { team: null }, betas: BETAS })
        .withResponse();
      assert.notEqual(removedMetadata.response.headers.get('etag'), beforeNoopEtag, 'U3 advances revision');
      assert.equal(removedMetadata.data.metadata.team, undefined, 'U3 deletes the metadata key');
      await client.beta.sessions.update(session.id, {
        metadata: { first: '1', second: '2' },
        betas: BETAS,
      });
      const clearedMetadata = await client.beta.sessions.update(session.id, {
        metadata: null,
        betas: BETAS,
      });
      assert.deepEqual(clearedMetadata.metadata, {}, 'U6 null clears the entire metadata bag');
      pass('U1-U3/U6 root update table distinguishes no-op, key deletion, and bag clear');

      // -- mid-session agent update gate ------------------------------------
      // Only tools and MCP servers are mutable. Arrays are full replacements;
      // model/system/skills are rejected instead of being silently ignored.
      const clearedTools = await client.beta.sessions.update(session.id, {
        agent: { tools: [] },
        betas: BETAS,
      });
      assert.deepEqual(clearedTools.agent.tools, [], 'tools update is a full replacement');
      const updatedAfterTools = await drain(client.beta.sessions.events.list(session.id, { betas: BETAS }));
      assert.ok(updatedAfterTools.some((e) => e.type === 'session.updated' && Array.isArray(e.agent?.tools)),
        'tools update emits session.updated');
      await assert.rejects(
        () => client.beta.sessions.update(session.id, { agent: { model: 'forbidden-model' }, betas: BETAS }),
        (err) => err.status === 400,
      );
      await assert.rejects(
        () => client.beta.sessions.update(session.id, { agent: { system: 'forbidden-system' }, betas: BETAS }),
        (err) => err.status === 400,
      );
      for (const [label, body, headers] of [
        ['agent must be an object', { agent: 'not-an-object' }, {}],
        ['immutable environment', { title: 'must-not-apply', environment_id: 'env_other' }, {}],
        ['empty idempotency key', { title: 'must-not-apply' }, { 'idempotency-key': '   ' }],
        ['malformed If-Match', { title: 'must-not-apply' }, { 'if-match': '7' }],
      ]) {
        const response = await rawUpdate(baseUrl, session.id, body, headers);
        assert.equal(response.status, 400, `${label} fails before root mutation: ${await response.text()}`);
      }
      const afterRejectedUpdates = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
      assert.notEqual(afterRejectedUpdates.title, 'must-not-apply', 'U4 rejected commands are effect-free');
      const wildcardUpdate = await rawUpdate(
        baseUrl,
        session.id,
        { title: 'wildcard-precondition' },
        { 'if-match': '*' },
      );
      assert.equal(wildcardUpdate.status, 200, `U5 wildcard precondition applies: ${await wildcardUpdate.text()}`);
      assert.equal(
        (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).title,
        'wildcard-precondition',
      );
      pass('session agent/update decision gate: valid tools replace; immutable and malformed inputs reject 400');

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

      await assert.rejects(
        () => client.beta.sessions.resources.update(res.id, {
          session_id: session.id,
          authorization_token: 'must-not-enter', // awaken-allow: secret
          betas: BETAS,
        }),
        (err) => err.status === 400,
        'the official raw-token update is rejected before resource mutation',
      );
      assert.equal(
        (await client.beta.sessions.resources.retrieve(res.id, {
          session_id: session.id,
          betas: BETAS,
        })).mount_path,
        '/workspace/in.txt',
        'rejected update leaves the durable mount unchanged',
      );
      pass('beta.sessions.resources list/retrieve and fail-closed update behavior');

      const delRes = await client.beta.sessions.resources.delete(res.id, {
        session_id: session.id,
        betas: BETAS,
      });
      assert.equal(delRes.type, 'session_resource_deleted');
      pass('beta.sessions.resources.delete');

      // -- archive + delete --------------------------------------------------
      const archived = await client.beta.sessions.archive(session.id, { betas: BETAS });
      assert.ok(archived.archived_at, 'archived session carries archived_at');
      await assert.rejects(
        () => client.beta.sessions.delete(session.id, { betas: BETAS }),
        (err) => err.status === 404,
        'U9 cannot rewrite an archived terminal fact as deleted',
      );

      const deletionCandidate = await client.beta.sessions.create({
        agent: 'assistant',
        title: 'delete candidate',
        betas: BETAS,
      });
      const del = await client.beta.sessions.delete(deletionCandidate.id, { betas: BETAS });
      assert.equal(del.type, 'session_deleted');
      await assert.rejects(
        () => client.beta.sessions.retrieve(deletionCandidate.id, { betas: BETAS }),
        (err) => err.status === 404,
      );
      assert.ok(
        (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).archived_at,
        'U9 rejected delete preserves the archived tombstone',
      );
      pass('U7-U9 archive/delete terminal decision table');
    });

    console.log('E2E PASS: the sessions family (update/list/delete/archive + threads + resources) round-trips through @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
