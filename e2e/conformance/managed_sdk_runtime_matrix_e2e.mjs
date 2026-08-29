// Supported/current/reviewed-candidate SDK × Native/ACP runtime compatibility matrix.
//
// Cause/effect graph: creator SDK -> one durable Session -> operator SDK ->
// selected runtime -> committed events -> terminal lifecycle. The SDK handoff
// must not change runtime selection, pagination, event identity, or cleanup.
// Decision table: every admitted creator SDK × every admitted operator SDK
// × {native,ACP}; every cell must create, retrieve, send, paginate, archive
// and delete through the generated Managed surface. Candidate admission is
// external to this shared suite and can only inject the exact reviewed alias.

import assert from 'node:assert/strict';
import {
  loadConformanceClients,
  projectsWorkspaceResponseContext,
} from '../../packages/managed-sdk-oracle/src/conformance/clients.mjs';
import { pass, waitForSessionEventReceipt, withScenarioServer } from '../harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CLIENTS = (await loadConformanceClients()).map(({ version, Client }) => [version, Client]);

async function drain(items) {
  const values = [];
  for await (const item of items) values.push(item);
  return values;
}

async function exerciseResponseContext(baseURL, clientSpec) {
  const [version, Client] = clientSpec;
  const client = new Client({ apiKey: 'e2e-dummy', baseURL, maxRetries: 0 });
  let session;
  try {
    // Test design: official_sdk_response_context_crosses_the_real_process
    //
    // Cause/effect graph:
    // exact official SDK anchor -> real Awaken HTTP edge -> authenticated
    // Workspace + request correlation -> raw Response, parsed DTO and typed
    // SDK error projections.
    //
    // Decision table:
    // | application result | SDK observation                                   |
    // | 2xx JSON           | request-id in raw/withResponse/parsed DTO          |
    // | 4xx JSON           | same request-id in raw headers and typed exception |
    // | scoped request     | anthropic-workspace-id remains non-empty           |
    //
    // This is the missing composite edge between the existing fake-transport
    // SDK contract and server-only envelope tests. Every exact TypeScript
    // anchor executes it, so a header renamed or dropped by composition fails.
    const created = await client.beta.sessions.create({
      agent: 'native-assistant',
      environment_id: 'env_local',
      betas: BETAS,
    }).withResponse();
    session = created.data;
    const requestID = created.response.headers.get('request-id');
    const workspaceID = created.response.headers.get('anthropic-workspace-id');
    assert.match(requestID ?? '', /^req_[0-9a-f]{32}$/u, `${version}: response request id`);
    assert.ok(workspaceID, `${version}: response workspace id`);
    assert.equal(created.request_id, requestID, `${version}: withResponse request id`);
    assert.equal(session._request_id, requestID, `${version}: parsed request id`);
    const projectsWorkspace = projectsWorkspaceResponseContext(version);
    assert.equal(
      'workspace_id' in created,
      projectsWorkspace,
      `${version}: reviewed withResponse workspace capability`,
    );
    assert.equal(
      '_workspace_id' in session,
      projectsWorkspace,
      `${version}: reviewed parsed workspace capability`,
    );
    if (projectsWorkspace) {
      assert.equal(created.workspace_id, workspaceID, `${version}: withResponse workspace id`);
      assert.equal(session._workspace_id, workspaceID, `${version}: parsed workspace id`);
    }

    await assert.rejects(
      () => client.beta.sessions.retrieve(`sesn_missing_${version.replaceAll('.', '_')}`, {
        betas: BETAS,
      }),
      (error) => {
        const errorRequestID = error.headers?.get('request-id');
        const errorWorkspaceID = error.headers?.get('anthropic-workspace-id');
        assert.equal(error.status, 404, `${version}: typed error status`);
        assert.equal(error.type, 'not_found_error', `${version}: typed error kind`);
        assert.match(errorRequestID ?? '', /^req_[0-9a-f]{32}$/u, `${version}: error request id`);
        assert.equal(error.requestID, errorRequestID, `${version}: promoted error request id`);
        assert.ok(errorWorkspaceID, `${version}: error workspace id`);
        assert.equal(
          'workspaceID' in error,
          projectsWorkspace,
          `${version}: reviewed error workspace capability`,
        );
        if (projectsWorkspace) {
          assert.equal(error.workspaceID, errorWorkspaceID, `${version}: promoted workspace id`);
        }
        return true;
      },
    );
  } finally {
    if (session) await client.beta.sessions.delete(session.id, { betas: BETAS });
  }
  pass(`official SDK ${version} observes real response context`);
}

async function exerciseCell(baseURL, runtime, creatorSpec, operatorSpec) {
  const [creatorVersion, Creator] = creatorSpec;
  const [operatorVersion, Operator] = operatorSpec;
  const creator = new Creator({ apiKey: 'e2e-dummy', baseURL });
  const operator = new Operator({ apiKey: 'e2e-dummy', baseURL });
  const agent = runtime === 'acp' ? 'acp-agent' : 'native-assistant';
  const session = await creator.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  try {
    const retrieved = await operator.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id, `${runtime}: cross-version retrieve`);
    const receipt = await operator.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `${runtime}-${creatorVersion}-to-${operatorVersion}` }],
      }],
      betas: BETAS,
    });
    // H1: C1=operator-version exact receipt; C2=creator-version observes the
    // selected runtime reply and idle. E1=cross-version committed handoff.
    // Constraint: auto-pagination remains a separate read oracle after C2.
    // C1&&!C2=>observe; C1+C2=>E1.
    await waitForSessionEventReceipt(
      creator,
      session.id,
      receipt.data[0]?.id,
      BETAS,
      ({ delta }) => {
        const texts = delta
          .filter((event) => event.type === 'agent.message')
          .flatMap((event) => event.content ?? [])
          .map((content) => content.text ?? '');
        return texts.some(runtime === 'acp'
          ? (text) => text.includes('acp-runtime reply')
          : (text) => text.startsWith('Echo:'))
          && delta.some((event) => event.type === 'session.status_idle');
      },
      `${runtime}: cross-version exact receipt reaches selected runtime reply`,
      { pollMs: 10 },
    );
    const events = await drain(creator.beta.sessions.events.list(session.id, {
      limit: 1,
      betas: BETAS,
    }));
    assert.equal(
      new Set(events.map((event) => event.id)).size,
      events.length,
      `${runtime}: pagination must not duplicate events`,
    );
    assert.ok(events.length > 1, `${runtime}: limit=1 auto-pagination crosses pages`);
    const archived = await creator.beta.sessions.archive(session.id, { betas: BETAS });
    assert.ok(archived.archived_at, `${runtime}: archive`);
    const deleted = await operator.beta.sessions.delete(session.id, { betas: BETAS });
    assert.equal(deleted.type, 'session_deleted', `${runtime}: delete`);
    pass(`${runtime} Session handoff ${creatorVersion} -> ${operatorVersion}`);
  } catch (error) {
    try { await creator.beta.sessions.delete(session.id, { betas: BETAS }); } catch {}
    throw error;
  }
}

await withScenarioServer('acp', 'echo', 38187, async (baseURL) => {
  for (const client of CLIENTS) await exerciseResponseContext(baseURL, client);
  for (const runtime of ['native', 'acp']) {
    for (const creator of CLIENTS) {
      for (const operator of CLIENTS) await exerciseCell(baseURL, runtime, creator, operator);
    }
  }
});

console.log(`E2E PASS: ${CLIENTS.length ** 2} SDK handoffs preserve Native and ACP Session behavior.`);
