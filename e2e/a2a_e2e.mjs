// A2A protocol e2e via the official @a2a-js/sdk `A2AClient` (JSON-RPC transport,
// resolved from the agent card). Covers multi-turn (the contextId threads history
// to the model) and multimodal (a file part reaches the model). Run: (from e2e/)
// npm install && node a2a_e2e.mjs

import assert from 'node:assert/strict';
import { A2AClient, ClientFactory } from '@a2a-js/sdk/client';
import {
  pass,
  publishAlwaysAskManagementProbeAgent,
  RED_PNG_B64,
  withRealServer,
  withScenarioServer,
} from './harness.mjs';

function replyText(res) {
  // `message/send` returns a Task; the agent's turn is its status message.
  const parts = res.result?.status?.message?.parts ?? [];
  return parts
    .filter((p) => p.kind === 'text')
    .map((p) => p.text)
    .join('');
}

async function main() {
  // --- multi-turn: a shared contextId threads the conversation ---
  await withRealServer('echo', 38151, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const modernClient = await new ClientFactory().createFromUrl(base, '/v1/a2a/agent-card');
    const extendedCard = await modernClient.getAgentCard();
    assert.equal(extendedCard.capabilities?.streaming, true);
    assert.equal(extendedCard.capabilities?.pushNotifications, true);
    assert.equal(extendedCard.supportsAuthenticatedExtendedCard, true);
    const r1 = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-mt',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'first message' }],
      },
    });
    assert.ok(replyText(r1).includes('first message'), `turn 1: ${replyText(r1)}`);
    const r2 = await client.sendMessage({
      message: {
        messageId: 'm2',
        contextId: 'a2a-mt',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'second message' }],
      },
    });
    assert.ok(replyText(r2).includes('second message'), `turn 2: ${replyText(r2)}`);

    // Causal graph:
    // A2A data JSON -> typed A2A Part -> runtime ACL (no generic JSON block)
    //                               `-> adjacent text -> model -> task reply
    // Decision table:
    // | part | accepted by A2A | reaches neutral prompt | observable reply |
    // | data | yes             | no                     | marker absent    |
    // | text | yes             | yes                    | text present     |
    // This drives the complete protocol/runtime path; it is not a DTO-only check.
    const withData = await client.sendMessage({
      message: {
        messageId: 'm3',
        contextId: 'a2a-data',
        role: 'user',
        kind: 'message',
        parts: [
          { kind: 'data', data: { nested: [1, true, null], marker: 'must-not-be-prompted' } },
          { kind: 'text', text: 'visible text' },
        ],
      },
    });
    assert.ok(replyText(withData).includes('visible text'), `text was lost: ${replyText(withData)}`);
    assert.ok(
      !replyText(withData).includes('must-not-be-prompted'),
      `A2A-owned JSON leaked into the neutral prompt: ${replyText(withData)}`,
    );

    // Tagged-union admission is behavioral: malformed parts fail before a task
    // or model turn exists, while the same context remains usable afterward.
    //
    // Decision table:
    // | discriminator | payload | expected effect |
    // | absent | text | -32602, no model turn |
    // | text | text + data | -32602, no model turn |
    // | file | bytes + uri | -32602, no model turn |
    // | message kind/id absent or unknown field | legal part | -32602, no model turn |
    // | data | object | accepted; data never enters prompt |
    for (const [id, part] of [
      [41, { text: 'kindless-must-not-run' }],
      [42, { kind: 'text', text: 'conflict-must-not-run', data: {} }],
      [43, { kind: 'file', file: { bytes: 'AAAA', uri: 'https://invalid.test/file' } }],
    ]) {
      const response = await fetch(`${base}/v1/a2a`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0', id, method: 'message/send',
          params: {
            message: {
              kind: 'message', messageId: `strict-${id}`, contextId: 'a2a-strict',
              role: 'user', parts: [part],
            },
          },
        }),
      });
      const rejected = await response.json();
      assert.equal(rejected.error?.code, -32602, JSON.stringify(rejected));
    }
    for (const [id, message] of [
      [44, { messageId: 'missing-kind', role: 'user', parts: [{ kind: 'text', text: 'x' }] }],
      [45, { kind: 'message', role: 'user', parts: [{ kind: 'text', text: 'x' }] }],
      [46, {
        kind: 'message', messageId: 'unknown-field', role: 'user', unexpected: true,
        parts: [{ kind: 'text', text: 'x' }],
      }],
    ]) {
      const response = await fetch(`${base}/v1/a2a`, {
        method: 'POST', headers: { 'content-type': 'application/json' },
        body: JSON.stringify({
          jsonrpc: '2.0', id, method: 'message/send', params: { message },
        }),
      });
      const rejected = await response.json();
      assert.equal(rejected.error?.code, -32602, JSON.stringify(rejected));
    }
    const invalidEnvelope = await fetch(`${base}/v1/a2a`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0', id: { illegal: true }, method: 'message/send',
        params: {
          message: {
            kind: 'message', messageId: 'invalid-id', role: 'user',
            parts: [{ kind: 'text', text: 'invalid-id-must-not-run' }],
          },
        },
      }),
    });
    assert.equal((await invalidEnvelope.json()).error?.code, -32600);
    const afterReject = await client.sendMessage({
      message: {
        messageId: 'strict-valid', contextId: 'a2a-strict', role: 'user', kind: 'message',
        parts: [{ kind: 'text', text: 'ONLY-VALID-TURN' }],
      },
    });
    assert.ok(replyText(afterReject).includes('ONLY-VALID-TURN'));
    for (const marker of ['kindless-must-not-run', 'conflict-must-not-run']) {
      assert.ok(!replyText(afterReject).includes(marker), `${marker} reached runtime`);
    }
    pass('a2a multi-turn + data-part isolation');
  });

  // --- HITL: a tool needing approval awaits the task (input-required); a follow-up
  // message on the same context carries an explicit structured approval and the
  // task completes; plain text is never interpreted as authorization ---
  await withScenarioServer('management-probe', 'probe', 38153, async (base) => {
    await publishAlwaysAskManagementProbeAgent(base, 'assistant', ['write'], ['read']);
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const awaiting = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-hitl',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'text', text: 'remember this note' }],
      },
    });
    assert.equal(
      awaiting.result?.status?.state,
      'input-required',
      `expected the write tool to await: ${JSON.stringify(awaiting.result?.status)}`,
    );
    const done = await client.sendMessage({
      message: {
        messageId: 'm2',
        contextId: 'a2a-hitl',
        role: 'user',
        kind: 'message',
        parts: [{ kind: 'data', data: { type: 'tool-approval', allow: true, note: 'reviewed' } }],
      },
    });
    assert.equal(done.result?.status?.state, 'completed', `expected completion after approval`);
    assert.ok(replyText(done).includes('done'), `expected the run to finish: ${replyText(done)}`);
    pass('a2a HITL approval (await -> approve -> complete)');
  });

  // Causal graph (official client task-control surface):
  // published AlwaysAsk -> durable input-required task -> get / subscribe / push-config owner
  //                                  |-> cancel -> runtime denial -> canceled event
  //                                  `-> config CRUD -> redacted reads -> deletion
  // Decision table:
  // | task exists | terminal | operation       | state/effect                         |
  // | yes         | no       | get             | exact durable input-required task    |
  // | yes         | no       | resubscribe      | current snapshot, then live cancel   |
  // | yes         | no       | cancel           | pending run denied; task canceled    |
  // | yes         | any      | push set/get/list| one config; credentials never echo   |
  // | yes         | any      | push delete      | config absent on the next read       |
  // | no          | -        | get/cancel       | stable SDK task-not-found failure    |
  await withScenarioServer('management-probe', 'probe', 38154, async (base) => {
    await publishAlwaysAskManagementProbeAgent(base, 'assistant', ['write'], ['read']);
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const awaiting = await client.sendMessage({
      message: {
        messageId: 'control-start', contextId: 'a2a-control', role: 'user', kind: 'message',
        parts: [{ kind: 'text', text: 'create controlled task' }],
      },
    });
    const taskId = awaiting.result?.id;
    assert.ok(taskId, JSON.stringify(awaiting));

    const fetched = await client.getTask({ id: taskId, historyLength: 1 });
    assert.equal(fetched.result?.id, taskId);
    assert.equal(fetched.result?.status?.state, 'input-required');
    assert.ok((fetched.result?.history?.length ?? 0) <= 1);
    const laxGet = await fetch(`${base}/v1/a2a`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        jsonrpc: '2.0', id: 99, method: 'tasks/get',
        params: { id: taskId, unexpectedSelector: true },
      }),
    });
    assert.equal((await laxGet.json()).error?.code, -32602);

    const set = await client.setTaskPushNotificationConfig({
      taskId,
      pushNotificationConfig: {
        id: 'control-hook', url: 'https://example.invalid/a2a-hook', token: 'must-not-echo', // awaken-allow: secret
        authentication: { schemes: ['Bearer'], credentials: 'must-not-echo-auth' }, // awaken-allow: secret
      },
    });
    assert.equal(set.result?.pushNotificationConfig?.id, 'control-hook');
    assert.equal(set.result?.pushNotificationConfig?.token, undefined);
    assert.equal(set.result?.pushNotificationConfig?.authentication?.credentials, undefined);
    const listed = await client.listTaskPushNotificationConfig({ id: taskId });
    assert.equal(listed.result?.length, 1);
    assert.equal(listed.result?.[0]?.pushNotificationConfig?.token, undefined);
    const gotConfig = await client.getTaskPushNotificationConfig({
      id: taskId, pushNotificationConfigId: 'control-hook',
    });
    assert.equal(gotConfig.result?.pushNotificationConfig?.id, 'control-hook');
    assert.equal(gotConfig.result?.pushNotificationConfig?.authentication?.credentials, undefined);

    const subscription = client.resubscribeTask({ id: taskId });
    const current = await subscription.next();
    assert.equal(current.value?.kind, 'task');
    assert.equal(current.value?.status?.state, 'input-required');
    const canceled = await client.cancelTask({ id: taskId });
    assert.equal(canceled.result?.status?.state, 'canceled');
    const canceledEvent = await subscription.next();
    assert.equal(canceledEvent.value?.kind, 'status-update');
    assert.equal(canceledEvent.value?.status?.state, 'canceled');
    assert.equal(canceledEvent.value?.final, true);

    await client.deleteTaskPushNotificationConfig({
      id: taskId, pushNotificationConfigId: 'control-hook',
    });
    const afterDelete = await client.listTaskPushNotificationConfig({ id: taskId });
    assert.deepEqual(afterDelete.result, []);

    for (const operation of [
      () => client.getTask({ id: 'missing-task' }),
      () => client.cancelTask({ id: 'missing-task' }),
    ]) {
      const response = await operation();
      assert.equal(response.error?.code, -32001, JSON.stringify(response));
    }
    pass('a2a official task control + subscription + redacted push configuration');
  });

  // Causal graph: official sendMessageStream -> server SSE -> ordered union events
  // -> terminal status. A dropped/invalid union member would make the SDK parser or
  // this state-order assertion fail.
  // Decision table:
  // | execution | first event | live output         | terminal event |
  // | succeeds  | working task| artifact-update(s)  | completed/final|
  await withRealServer('echo', 38155, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const events = [];
    for await (const event of client.sendMessageStream({
      message: {
        messageId: 'stream-start', contextId: 'a2a-stream', role: 'user', kind: 'message',
        parts: [{ kind: 'text', text: 'stream-visible' }],
      },
    })) {
      events.push(event);
    }
    assert.equal(events[0]?.kind, 'task', JSON.stringify(events));
    assert.equal(events[0]?.status?.state, 'working');
    assert.ok(events.some((event) => event.kind === 'artifact-update'));
    const terminal = events.at(-1);
    assert.equal(terminal?.kind, 'status-update');
    assert.equal(terminal?.status?.state, 'completed');
    assert.equal(terminal?.final, true);
    pass('a2a official streaming union ordering and terminal state');
  });

  // --- multimodal: a `file` image part travels to the model ---
  await withRealServer('vision', 38152, async (base) => {
    const client = await A2AClient.fromCardUrl(`${base}/v1/a2a/agent-card`);
    const res = await client.sendMessage({
      message: {
        messageId: 'm1',
        contextId: 'a2a-img',
        role: 'user',
        kind: 'message',
        parts: [
          { kind: 'file', file: { bytes: RED_PNG_B64, mimeType: 'image/png' } },
          { kind: 'text', text: 'what color is this' },
        ],
      },
    });
    assert.ok(replyText(res).includes('image/png'), `image did not reach the model: ${replyText(res)}`);
    pass('a2a multimodal (image reached the model)');
  });

  console.log('E2E PASS: A2A messages, strict admission, task control, push config, streaming, multimodal and HITL via @a2a-js/sdk.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
