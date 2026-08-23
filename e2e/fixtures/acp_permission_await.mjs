import assert from 'node:assert/strict';
import { waitForSessionEventReceipt } from '../harness.mjs';

// One official-SDK observer for the deterministic ACP permission fixture. It
// owns no Runtime identity or permission decision: the qualified public tool id
// and requires_action edge are read from the exact User receipt delta.
export async function startAcpPermissionAwait(client, betas) {
  const session = await client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: 'env_local',
    betas,
  });
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'request permission' }] }],
    betas,
  });
  const receiptId = receipt.data?.[0]?.id;
  assert.equal(typeof receiptId, 'string', 'official SDK returns the exact User Event receipt');
  const { delta: observed } = await waitForSessionEventReceipt(
    client,
    session.id,
    receiptId,
    betas,
    ({ delta }) => {
      const tool = delta.find(
        (event) => event.type === 'agent.tool_use' && event.evaluated_permission === 'ask',
      );
      return tool !== undefined && delta.some(
        (event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'requires_action'
          && event.stop_reason.event_ids.includes(tool.id),
      );
    },
    'ACP permission to reach its qualified requires_action boundary',
  );
  const tool = observed.find(
    (event) => event.type === 'agent.tool_use' && event.evaluated_permission === 'ask',
  );
  assert.ok(tool, `ACP permission request projected as a tool use: ${JSON.stringify(observed)}`);
  assert.equal(tool.name, 'bash');
  assert.equal(tool.evaluated_permission, 'ask');
  const idle = observed.find(
    (event) => event.type === 'session.status_idle'
      && event.stop_reason?.type === 'requires_action',
  );
  assert.equal(idle?.stop_reason?.type, 'requires_action');
  assert.deepEqual(idle.stop_reason.event_ids, [tool.id]);
  return { session, tool, observed };
}
