// Standard Webhooks differential against the official Anthropic SDK parser.
// The body/header construction is byte-for-byte the Rust awaken-webhook
// contract; no custom verifier participates in the assertions.
// Cause/effect graph: exact Rust-compatible Session or Deployment-run lifecycle
// body + fresh signed headers -> unwrap; payload mutation or absent verification
// key -> reject before event handling.
// Decision table: W1={Session bytes,valid key}->accept; W2={Deployment-run
// bytes,valid key}->accept exact public union; W3={tampered bytes,valid key} or
// {valid bytes,missing key}->reject. The product-side outbox test owns actual
// lifecycle emission and retries; this suite owns only official SDK decoding.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import AnthropicCurrent from '@anthropic-ai/sdk';
import Anthropic0105 from '@anthropic-ai/sdk-0-105';
import Anthropic0117 from '@anthropic-ai/sdk-0-117';
import { sdkVersionBinding } from './conformance/catalog.mjs';
import { pass } from './harness.mjs';

const keyBytes = Buffer.from('managed-webhook-test-key!');
const key = `whsec_${keyBytes.toString('base64')}`;
const timestamp = String(Math.floor(Date.now() / 1000));
const CURRENT_SDK_VERSION = sdkVersionBinding().oracle;
const eventCases = [{
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

function signedEvent(data, index) {
  const id = `event_official_sdk_${index}`;
  const event = {
    type: 'event',
    id,
    created_at: new Date(Number(timestamp) * 1000).toISOString(),
    data,
  };
  const body = JSON.stringify(event);
  const signature = crypto
    .createHmac('sha256', keyBytes)
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

for (const [version, Anthropic] of [
  ['0.105.0', Anthropic0105],
  ['0.117.1', Anthropic0117],
  [CURRENT_SDK_VERSION, AnthropicCurrent],
]) {
  const client = new Anthropic({ apiKey: 'inert', webhookKey: key });
  for (const [index, data] of eventCases.entries()) {
    const { event, body, headers } = signedEvent(data, index);
    const parsed = client.beta.webhooks.unwrap(body, { headers });
    assert.deepEqual(parsed, event);
    pass(`official SDK ${version} webhooks.unwrap accepts ${data.type}`);
  }

  const { body, headers } = signedEvent(eventCases[0], 0);
  assert.throws(
    () => client.beta.webhooks.unwrap(body.replace('thread_1', 'tampered'), { headers }),
    /signature/i,
    'a payload mutation must fail official verification',
  );
  assert.throws(
    () => new Anthropic({ apiKey: 'inert' }).beta.webhooks.unwrap(body, { headers }),
    /Webhook key must not be null/,
    'verification must fail closed when no key is configured',
  );
}
pass('all pinned official SDKs webhooks.unwrap reject tampering and missing key');
