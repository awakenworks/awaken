// Org admission e2e: every HTTP protocol entry of one real awaken process admits
// through the same Org bucket, and each protocol refuses in its own wire shape.
//
// Decision table (fresh process, organization Create bucket = 3/minute, Read =
// 1,200/minute, injected only through the scenario composition's existing
// request-limiter port):
// | Rule | entry            | bucket state         | effect |
// | O1   | Managed create   | 3 -> 2 -> 1          | admitted (Session, application token) |
// | O2   | AI SDK run       | 1 -> 0               | admitted (200 stream) |
// | O3   | AI SDK run       | exhausted            | 429 plain text + retry-after, no Anthropic headers |
// | O4   | Managed create   | exhausted            | 429 Anthropic rate_limit_error + anthropic-ratelimit-* |
// | O5   | A2A JSON-RPC     | exhausted            | 429 JSON-RPC error, id null + retry-after |
// | O6   | AI SDK read      | Read bucket untouched| admitted (independent Read bucket) |
//
// Run: (from e2e/) node org_admission_shared_bucket_e2e.mjs

import assert from 'node:assert/strict';
import {
  createCrossProtocolApplicationThread,
  pass,
  publishManagementAgent,
  withScenarioServer,
} from './harness.mjs';

const BETA = 'managed-agents-2026-04-01';

async function main() {
  try {
    await withScenarioServer(
      'management-rate-limit',
      'mcp',
      Number(process.env.E2E_PORT ?? 38631),
      async (base) => {
        // Config-plane authoring is not a metered protocol entry.
        await publishManagementAgent(base, 'assistant', {
          name: 'Org admission fixture',
          model: {
            mode: 'pinned',
            provider_identity_ref: 'default',
            model_ref: 'management',
            backend_ref: 'default',
          },
        });
        const { threadId, headers } = await createCrossProtocolApplicationThread(base);
        pass('O1 Managed Session create and application-token mint consume two Create tokens');

        const run = (text) => fetch(`${base}/v1/ai-sdk/threads/${threadId}/runs`, {
          method: 'POST',
          headers: { ...headers, 'content-type': 'application/json' },
          body: JSON.stringify({
            threadId,
            messages: [{ id: `u-${text}`, role: 'user', parts: [{ type: 'text', text }] }],
          }),
        });
        const admitted = await run('first');
        assert.equal(admitted.status, 200, 'O2 the last Create token admits the AI SDK run');
        await admitted.text();
        pass('O2 AI SDK run consumes the last token of the shared Org bucket');

        const refusedRun = await run('second');
        assert.equal(refusedRun.status, 429, 'O3');
        assert.ok(Number(refusedRun.headers.get('retry-after')) >= 1, 'O3 retry-after');
        assert.equal(refusedRun.headers.get('anthropic-ratelimit-requests-limit'), null, 'O3');
        assert.match(await refusedRun.text(), /request limit exceeded/, 'O3 plain text');
        pass('O3 AI SDK refuses in its own plain-text shape once Managed spent the bucket');

        const refusedManaged = await fetch(`${base}/v1/sessions`, {
          method: 'POST',
          headers: { 'anthropic-beta': BETA, 'content-type': 'application/json' },
          body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
        });
        assert.equal(refusedManaged.status, 429, 'O4');
        assert.equal(refusedManaged.headers.get('anthropic-ratelimit-requests-limit'), '3', 'O4');
        assert.equal(refusedManaged.headers.get('anthropic-ratelimit-requests-remaining'), '0', 'O4');
        const envelope = await refusedManaged.json();
        assert.equal(envelope.type, 'error', 'O4');
        assert.equal(envelope.error?.type, 'rate_limit_error', 'O4');
        pass('O4 Managed refuses in the Anthropic envelope once AI SDK spent the bucket');

        const refusedRpc = await fetch(`${base}/v1/a2a`, {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            jsonrpc: '2.0',
            id: 1,
            method: 'message/send',
            params: {
              message: {
                messageId: 'org-admission',
                contextId: threadId,
                role: 'user',
                kind: 'message',
                parts: [{ kind: 'text', text: 'third protocol' }],
              },
            },
          }),
        });
        assert.equal(refusedRpc.status, 429, 'O5');
        assert.ok(Number(refusedRpc.headers.get('retry-after')) >= 1, 'O5 retry-after');
        const rpc = await refusedRpc.json();
        assert.equal(rpc.jsonrpc, '2.0', 'O5');
        assert.equal(rpc.id, null, 'O5');
        assert.equal(typeof rpc.error?.code, 'number', 'O5');
        pass('O5 A2A refuses with a JSON-RPC error on the same exhausted bucket');

        const read = await fetch(`${base}/v1/ai-sdk/threads/${threadId}/messages`, { headers });
        assert.equal(read.status, 200, 'O6 the Read bucket is independent');
        pass('O6 reads still pass through the independent Read bucket');
      },
    );
    console.log('E2E PASS: every protocol entry shares one Org bucket and refuses in its own shape.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
