// The rest of the sessions family, driven by the official Anthropic TypeScript
// SDK (`client.beta.sessions.*`): update / list / delete / archive, the
// `threads` subresource (list / retrieve / archive), and the `resources`
// subresource (add / list / retrieve / update / delete). The create / retrieve /
// events path is covered elsewhere; this exercises the endpoints that were
// missing so any wire-shape drift surfaces as an SDK decode error.
// Causes: root update/CAS fields, disposition command, budget state, and
// Thread/resource subresource operation are partitioned by the tables below.
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
// Terminal-disposition cause graph: C7 archive and delete are distinct retention
// commands; C8 execution termination is orthogonal to the disposition axis.
// Effects: U7 archive retains a terminated tombstone; U8 delete removes a live
// aggregate; U9 archive-then-delete advances only the disposition from archived
// to deleting while preserving the already-terminal execution fact. This table
// prevents the protocol from reintroducing a competing single-axis state machine.
//
// | Rule | Initial state | Command | Effect |
// |---|---|---|---|
// | U7 | live | archive | terminated tombstone with `archived_at` |
// | U8 | live | delete | `session_deleted`; retrieve returns 404 |
// | U9 | archived + terminated | delete | `session_deleted`; retrieve returns 404 |
//
// Budget cause graph: C10 budget is fixed at creation but its existing cap may
// be raised/lowered or removed; C11 a Session created without a budget cannot
// acquire one later; C12 wire amounts are canonical positive integer cents and
// the Scenario snapshot charges 14 cents per cache-normalized Provider request.
// C13 the first logical request reaches the cap, then an exact MCP confirmation
// allows the same Run's post-tool request; C14 the cap is raised or removed;
// C15 a System Event trails that confirmation even though confirmations are not
// valid System predecessors; C18 lifecycle attempt boundaries may project
// adjacent `running` observations without duplicating a domain effect. Effects are U10 exact SDK
// create/retrieve/list projection, U11 exact update or removal, U12 rejection
// before root mutation, U13 one budget pause, and U14 automatic continuation of
// the same Run without a second User Event or duplicated tool effect; U15 is a
// 400 atomic no-op before the later exact confirmation succeeds; U17 collapses
// only adjacent equal observations when checking semantic status phases while
// the exact Event/effect cardinalities remain independently asserted. Canonical
// replay may place the preceding requires-action idle between a usage snapshot
// and the later budget idle, so U13 requires relative order plus one budget
// terminal rather than cross-phase physical adjacency.
//
// | Rule | Created budget | Update | Effect |
// |---|---|---|---|
// | B1 | 100 USD cents | none | all Session reads echo 100 USD |
// | B2 | 100 | 250 | root and subsequent reads echo 250 USD |
// | B3 | 250 | null | budget is removed |
// | B4 | absent | 100 | 400; budget remains absent |
// | B5 | 100 | noncanonical/zero | 400; budget remains 100 |
// | B6 | 1, exact MCP confirmation reaches next request | 100 | one resume; final end_turn; cost 28 |
// | B7 | 1, exact MCP confirmation reaches next request | null | one resume; final end_turn; cost 28 |
// | B8 | 1, MCP confirmation + trailing System | any | 400; no receipt/effect; pending unchanged |
// Resource cause graph: C16 the current Files upload contract carries only the
// file plus the Managed beta selector; C17 the resulting exact file id is then
// attached to the Session at one mount path. Effect U16 is one persisted File
// followed by one resolvable Session resource. Constraint K16: the retired
// multipart `purpose` field is not reintroduced by this Session-family test;
// Files owns upload validation and Sessions owns only attachment. Decision R16:
// C16 + C17 -> U16; malformed/extra multipart fields remain covered by the
// Files endpoint's rejection table.
// Constraints/invariant: one Session root revision owns update/CAS/disposition/
// budget; Thread, resource, and Event subresources project from that same
// aggregate and no rejected arm emits a partial Event or Run.
// Decision rules are U1-U15/B1-B8 above; their effects distinguish semantic
// no-op, retained archive, deletion, budget pause/resume, and atomic rejection.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import {
  withScenarioServer,
  pass,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

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

const sessionEvents = (client, sessionId) =>
  drain(client.beta.sessions.events.list(sessionId, { betas: BETAS }));

function semanticThreadStatusPhases(events, threadId) {
  return events
    .filter((event) =>
      event.session_thread_id === threadId && event.type.startsWith('session.thread_status_'))
    .map((event) =>
      event.type === 'session.thread_status_idle'
        ? `${event.type}:${event.stop_reason.type}`
        : event.type)
    .filter((status, index, statuses) => index === 0 || status !== statuses[index - 1]);
}

async function exerciseBudgetResume(client, updateKind) {
  const budgeted = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    budget: { type: 'limit', max_list_cost: { amount: '1', currency: 'USD' } },
    betas: BETAS,
  });
  const taskReceipt = await client.beta.sessions.events.send(budgeted.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'add 2 3' }] }],
    betas: BETAS,
  });
  const { events: awaitingTool } = await waitForSessionEventReceipt(
    client,
    budgeted.id,
    taskReceipt.data[0]?.id,
    BETAS,
    ({ delta }) => delta.some((event) =>
      event.type === 'session.status_idle' && event.stop_reason.type === 'requires_action'),
    `${updateKind} reaches the client tool boundary`,
  );
  const mcpToolUse = awaitingTool.find((event) => event.type === 'agent.mcp_tool_use');
  assert.ok(mcpToolUse?.id, `${updateKind} exposes one public MCP Event id`);
  const beforeInvalidIds = awaitingTool.map((event) => event.id);
  await assert.rejects(
    client.beta.sessions.events.send(budgeted.id, {
      events: [{
        type: 'user.tool_confirmation',
        tool_use_id: mcpToolUse.id,
        result: 'allow',
      }, {
        type: 'system.message',
        content: [{ type: 'text', text: 'must not trail a confirmation' }],
      }],
      betas: BETAS,
    }),
    (error) => error?.status === 400,
    `${updateKind} B8 rejects Confirmation+System`,
  );
  assert.deepEqual(
    (await sessionEvents(client, budgeted.id)).map((event) => event.id),
    beforeInvalidIds,
    `${updateKind} B8 appends neither confirmation nor System`,
  );
  const confirmationReceipt = await client.beta.sessions.events.send(budgeted.id, {
    events: [{
      type: 'user.tool_confirmation',
      tool_use_id: mcpToolUse.id,
      result: 'allow',
    }],
    betas: BETAS,
  });
  const { events: paused } = await waitForSessionEventReceipt(
    client,
    budgeted.id,
    confirmationReceipt.data[0]?.id,
    BETAS,
    ({ delta }) => delta.some((event) =>
      event.type === 'session.status_idle' && event.stop_reason.type === 'budget_reached'),
    `${updateKind} reaches the request gate`,
  );
  const threads = await drain(client.beta.sessions.threads.list(budgeted.id, { betas: BETAS }));
  const primary = threads.find((thread) => thread.parent_thread_id == null);
  assert.ok(primary?.id.startsWith('sthr_'), `${updateKind} has one public primary Thread`);
  assert.deepEqual(
    semanticThreadStatusPhases(paused, primary.id),
    [
      'session.thread_status_running',
      'session.thread_status_idle:requires_action',
      'session.thread_status_running',
      'session.thread_status_idle:budget_reached',
    ],
    `${updateKind} exact tool reply reaches one budget pause in the same Run`,
  );
  const pausedAggregate = paused.findIndex((event) =>
    event.type === 'session.status_idle' && event.stop_reason.type === 'budget_reached');
  const budgetUsage = paused
    .slice(0, pausedAggregate)
    .findLastIndex((event) => event.type === 'session.usage');
  assert.ok(budgetUsage >= 0, `${updateKind} usage precedes budget idle`);
  assert.equal(
    paused.filter((event) =>
      event.type === 'session.status_idle' && event.stop_reason.type === 'budget_reached').length,
    1,
    `${updateKind} has one budget terminal for ${budgeted.id}: ${JSON.stringify(paused
      .filter((event) => event.type === 'session.status_idle')
      .map((event) => ({ id: event.id, stop_reason: event.stop_reason.type })))}`,
  );
  assert.equal(
    paused.filter((event) => event.type === 'user.message').length,
    1,
    `${updateKind} has one User Event`,
  );

  await client.beta.sessions.update(budgeted.id, {
    budget: updateKind === 'raise'
      ? { type: 'limit', max_list_cost: { amount: '100', currency: 'USD' } }
      : null,
    betas: BETAS,
  });
  const completed = await waitForValue(
    () => sessionEvents(client, budgeted.id),
    (events) => events.some((event) =>
      event.type === 'session.status_idle' && event.stop_reason.type === 'end_turn') &&
      events.some((event) => event.type === 'agent.message'),
    `${updateKind} resumes the paused Run`,
  );
  assert.deepEqual(
    semanticThreadStatusPhases(completed, primary.id),
    [
      'session.thread_status_running',
      'session.thread_status_idle:requires_action',
      'session.thread_status_running',
      'session.thread_status_idle:budget_reached',
      'session.thread_status_running',
      'session.thread_status_idle:end_turn',
    ],
    `${updateKind} continues the same paused lifecycle once`,
  );
  assert.equal(completed.filter((event) => event.type === 'user.message').length, 1);
  assert.equal(completed.filter((event) => event.type === 'user.tool_confirmation').length, 1);
  assert.equal(completed.filter((event) => event.type === 'agent.mcp_tool_use').length, 1);
  assert.equal(completed.filter((event) => event.type === 'agent.mcp_tool_result').length, 1);
  assert.equal(completed.filter((event) => event.type === 'agent.message').length, 1);
  const finalAggregate = completed.findLastIndex((event) => event.type === 'session.status_idle');
  assert.equal(completed[finalAggregate - 1]?.type, 'session.usage', `${updateKind} final usage precedes idle`);
  const retrieved = await client.beta.sessions.retrieve(budgeted.id, { betas: BETAS });
  const terminalUsage = completed.slice(0, finalAggregate)
    .findLast((event) => event.type === 'session.usage');
  assert.equal(
    terminalUsage?.usage?.list_cost?.amount,
    '28',
    `${updateKind} final usage prices both logical requests`,
  );
  assert.equal(
    retrieved.usage?.list_cost?.amount,
    '28',
    `${updateKind} prices two logical requests: ${JSON.stringify({
      retrieved,
      usageEvents: completed.filter((event) => event.type === 'session.usage'),
    })}`,
  );
  if (updateKind === 'remove') {
    assert.equal(retrieved.budget, null, 'remove keeps the one-way removal');
  } else {
    assert.equal(retrieved.budget?.max_list_cost.amount, '100', 'raise keeps the new cap');
  }
  await client.beta.sessions.delete(budgeted.id, { betas: BETAS });
}

async function main() {
  try {
    await withScenarioServer('management', 'mcp', 38146, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // Session Agent-tool admission cause/effect graph: C1 create carries an
      // unknown member in agent_with_overrides; C2 update carries the same
      // unknown member beside otherwise valid root fields. Effects: E1 400 from
      // the shared validator; E2 create adds no Session; E3 update changes no
      // root revision, tools, title, or event. Decision rows: S1 C1=>E1+E2;
      // S2 C2=>E1+E3. K/Constraint: both ingress paths reject before the single
      // Session aggregate, so normalization never silently drops the member.
      const sessionsBeforeUnknownCreate = (await drain(
        client.beta.sessions.list({ betas: BETAS }),
      )).map((candidate) => candidate.id).sort();
      await assert.rejects(
        () => client.beta.sessions.create({
          agent: {
            id: 'assistant',
            type: 'agent_with_overrides',
            tools: [{
              type: 'agent_toolset_20260401',
              configs: [{ name: 'parallel_web_search' }],
            }],
          },
          environment_id: 'env_local',
          betas: BETAS,
        }),
        (error) => error?.status === 400,
        'S1/E1 unknown create-time member is rejected',
      );
      assert.deepEqual(
        (await drain(client.beta.sessions.list({ betas: BETAS })))
          .map((candidate) => candidate.id)
          .sort(),
        sessionsBeforeUnknownCreate,
        'S1/E2 rejection creates no Session',
      );

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
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
      const beforeUnknownUpdate = await client.beta.sessions
        .retrieve(session.id, { betas: BETAS })
        .withResponse();
      const beforeUnknownUpdateEvents = (await sessionEvents(client, session.id)).length;
      const unknownToolUpdate = await rawUpdate(baseUrl, session.id, {
        title: 'must-not-apply-unknown-tool',
        agent: {
          tools: [{
            type: 'agent_toolset_20260401',
            configs: [{ name: 'parallel_web_search' }],
          }],
        },
      });
      assert.equal(unknownToolUpdate.status, 400, 'S2/E1');
      assert.match(await unknownToolUpdate.text(), /unknown agent tool `parallel_web_search`/u, 'S2/E1');
      const afterUnknownUpdate = await client.beta.sessions
        .retrieve(session.id, { betas: BETAS })
        .withResponse();
      assert.equal(
        afterUnknownUpdate.response.headers.get('etag'),
        beforeUnknownUpdate.response.headers.get('etag'),
        'S2/E3 root revision is unchanged',
      );
      assert.equal(afterUnknownUpdate.data.title, beforeUnknownUpdate.data.title, 'S2/E3 title');
      assert.deepEqual(afterUnknownUpdate.data.agent.tools, beforeUnknownUpdate.data.agent.tools, 'S2/E3 tools');
      assert.equal(
        (await sessionEvents(client, session.id)).length,
        beforeUnknownUpdateEvents,
        'S2/E3 no Session Event is appended',
      );
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

      // -- budget lifecycle -------------------------------------------------
      const budgeted = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        budget: {
          type: 'limit',
          max_list_cost: { amount: '100', currency: 'USD' },
        },
        betas: BETAS,
      });
      assert.deepEqual(
        budgeted.budget,
        { type: 'limit', max_list_cost: { amount: '100', currency: 'USD' } },
        'B1 create echoes the public cent amount',
      );
      assert.deepEqual(
        (await client.beta.sessions.retrieve(budgeted.id, { betas: BETAS })).budget,
        budgeted.budget,
        'B1 retrieve uses the same budget projection',
      );
      const listedBudgeted = (await drain(client.beta.sessions.list({ betas: BETAS })))
        .find((candidate) => candidate.id === budgeted.id);
      assert.deepEqual(listedBudgeted?.budget, budgeted.budget, 'B1 list uses the same projection');

      const raisedBudget = await client.beta.sessions.update(budgeted.id, {
        budget: {
          type: 'limit',
          max_list_cost: { amount: '250', currency: 'USD' },
        },
        betas: BETAS,
      });
      assert.equal(raisedBudget.budget?.max_list_cost.amount, '250', 'B2 raises the existing cap');

      for (const invalid of ['0', '01']) {
        const response = await rawUpdate(baseUrl, budgeted.id, {
          budget: {
            type: 'limit',
            max_list_cost: { amount: invalid, currency: 'USD' },
          },
        });
        assert.equal(response.status, 400, `B5 rejects amount ${invalid}: ${await response.text()}`);
      }
      assert.equal(
        (await client.beta.sessions.retrieve(budgeted.id, { betas: BETAS })).budget?.max_list_cost.amount,
        '250',
        'B5 invalid updates leave the root unchanged',
      );

      const removedBudget = await client.beta.sessions.update(budgeted.id, {
        budget: null,
        betas: BETAS,
      });
      assert.equal(removedBudget.budget, null, 'B3 removes the existing budget');
      await assert.rejects(
        () => client.beta.sessions.update(session.id, {
          budget: {
            type: 'limit',
            max_list_cost: { amount: '100', currency: 'USD' },
          },
          betas: BETAS,
        }),
        (err) => err.status === 400,
        'B4 cannot add a budget after Session creation',
      );
      assert.equal(
        (await client.beta.sessions.retrieve(session.id, { betas: BETAS })).budget,
        null,
        'B4 rejection leaves the no-budget root unchanged',
      );
      pass('B1-B5 budget create/update/remove decision table through the official SDK');

      await exerciseBudgetResume(client, 'raise');
      await exerciseBudgetResume(client, 'remove');
      pass('B6-B7 budget pause + raise/remove automatically resumes one existing Run');

      // -- threads -----------------------------------------------------------
      const threads = await drain(client.beta.sessions.threads.list(session.id, { betas: BETAS }));
      assert.equal(threads.length, 1, 'a fresh session has one primary thread');
      const thread = threads[0];
      assert.equal(thread.type, 'session_thread');
      assert.equal(thread.session_id, session.id);
      assert.match(thread.id, /^sthr_/u, 'the primary Thread uses the public ID codec');
      assert.notEqual(thread.id, session.id, 'the internal root key is not a public Thread id');
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
      const archivedDelete = await client.beta.sessions.delete(session.id, { betas: BETAS });
      assert.equal(archivedDelete.type, 'session_deleted');
      await assert.rejects(
        () => client.beta.sessions.retrieve(session.id, { betas: BETAS }),
        (err) => err.status === 404,
        'U9 archived disposition advances to hidden deletion',
      );

      const deletionCandidate = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        title: 'delete candidate',
        betas: BETAS,
      });
      const del = await client.beta.sessions.delete(deletionCandidate.id, { betas: BETAS });
      assert.equal(del.type, 'session_deleted');
      await assert.rejects(
        () => client.beta.sessions.retrieve(deletionCandidate.id, { betas: BETAS }),
        (err) => err.status === 404,
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
