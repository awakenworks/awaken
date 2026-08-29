import assert from 'node:assert/strict';
import crypto from 'node:crypto';

const KEY_BYTES = Buffer.from('managed-webhook-test-key!');
const KEY = `whsec_${KEY_BYTES.toString('base64')}`;
const EVENT_CASES = [{
  type: 'session.thread_idled',
  id: 'session_1',
  session_thread_id: 'thread_1',
  organization_id: 'org_1',
  workspace_id: 'workspace_1',
}, {
  type: 'deployment_run.succeeded',
  id: 'drun_1',
  organization_id: 'org_1',
  workspace_id: 'workspace_1',
}];

function signedEvent(data, index, timestamp) {
  const id = `event_official_sdk_${index}`;
  const event = {
    type: 'event',
    id,
    created_at: new Date(Number(timestamp) * 1_000).toISOString(),
    data,
  };
  const body = JSON.stringify(event);
  const signature = crypto
    .createHmac('sha256', KEY_BYTES)
    .update(`${id}.${timestamp}.${body}`)
    .digest('base64');
  return {
    event,
    body,
    headers: {
      'webhook-id': id,
      'webhook-timestamp': timestamp,
      'webhook-signature': `v1,${signature}`,
    },
  };
}

export function exerciseOfficialWebhookContract(Client) {
  // Cause/effect graph: C1 exact body has a fresh valid signature; C2 body is
  // mutated; C3 webhook key is absent; C4 the SDK exposes parseUnverified.
  // Effects: E1 unwrap returns the exact public event union; E2 C2/C3 reject;
  // E3 C4 parses exact bytes without signature authority and malformed JSON
  // rejects. Decision rules W1 C1->E1; W2 C2||C3->E2; W3 C4->E3.
  // Constraints: this helper invokes only official SDK parsers and owns no
  // product route, event projection, verification fallback, or version branch.
  const timestamp = String(Math.floor(Date.now() / 1_000));
  const client = new Client({ apiKey: 'inert', webhookKey: KEY });
  const helpers = Object.getOwnPropertyNames(Object.getPrototypeOf(client.beta.webhooks))
    .filter((name) => name !== 'constructor' && typeof client.beta.webhooks[name] === 'function')
    .sort();
  assert.deepEqual(
    helpers,
    helpers.includes('parseUnverified') ? ['parseUnverified', 'unwrap'] : ['unwrap'],
    'every generated Webhook helper requires an explicit invocation below',
  );
  for (const [index, data] of EVENT_CASES.entries()) {
    const { event, body, headers } = signedEvent(data, index, timestamp);
    assert.deepEqual(client.beta.webhooks.unwrap(body, { headers }), event, 'W1/E1');
  }

  const { event, body, headers } = signedEvent(EVENT_CASES[0], 0, timestamp);
  assert.throws(
    () => client.beta.webhooks.unwrap(body.replace('thread_1', 'tampered'), { headers }),
    /signature/i,
    'W2/E2 mutated payload',
  );
  assert.throws(
    () => new Client({ apiKey: 'inert' }).beta.webhooks.unwrap(body, { headers }),
    /Webhook key must not be null/,
    'W2/E2 missing key',
  );

  const hasParseUnverified = typeof client.beta.webhooks.parseUnverified === 'function';
  if (hasParseUnverified) {
    assert.deepEqual(client.beta.webhooks.parseUnverified(body), event, 'W3/E3');
    assert.throws(
      () => client.beta.webhooks.parseUnverified('{'),
      undefined,
      'W3/E3 malformed JSON',
    );
  }
  return Object.freeze({ parseUnverified: hasParseUnverified });
}
