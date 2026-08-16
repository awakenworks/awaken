// Standard Webhooks differential against the official Anthropic SDK parser.
// The body/header construction is byte-for-byte the Rust awaken-webhook
// contract; no custom verifier participates in the assertions.
// Cause/effect graph: exact Rust-compatible body + fresh signed headers -> unwrap;
// payload mutation or absent verification key -> reject before event handling.
// Decision table: {valid bytes, valid key}=accept; {tampered bytes, valid key} or
// {valid bytes, missing key}=reject.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { pass } from './harness.mjs';

const keyBytes = Buffer.from('managed-webhook-test-key!');
const key = `whsec_${keyBytes.toString('base64')}`;
const client = new Anthropic({ apiKey: 'inert', webhookKey: key });
const timestamp = String(Math.floor(Date.now() / 1000));
const id = 'event_official_sdk';
const event = {
  type: 'event',
  id,
  created_at: new Date(Number(timestamp) * 1000).toISOString(),
  data: {
    type: 'session.thread_idled',
    id: 'session_1',
    session_thread_id: 'thread_1',
    organization_id: 'org_1',
    workspace_id: 'workspace_1',
  },
};
const body = JSON.stringify(event);
const signature = crypto
  .createHmac('sha256', keyBytes)
  .update(`${id}.${timestamp}.${body}`)
  .digest('base64');
const headers = {
  'webhook-id': id,
  'webhook-timestamp': timestamp,
  'webhook-signature': `v1,${signature}`,
};

const parsed = client.beta.webhooks.unwrap(body, { headers });
assert.deepEqual(parsed, event);
pass('official webhooks.unwrap accepts awaken Standard Webhooks bytes');

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
pass('official webhooks.unwrap rejects tampering and missing key');
