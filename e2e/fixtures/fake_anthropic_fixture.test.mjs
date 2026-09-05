import assert from 'node:assert/strict';
import test from 'node:test';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fake_anthropic_fixture.mjs';

test('one fixture serves discovery, token probes and inference with the same authentication', async () => {
  // Decision table W1-W6: three routes x valid/invalid credential. Only valid
  // inference invokes the callback; discovery/token probes do not advance the
  // inference sequence or consume injected faults. Query params preserve route
  // identity. Unauthorized calls return 401 and never invoke test behavior.
  const seen = [];
  const upstream = await startFakeAnthropic('fixture-key', {
    models:['fixture-model'], behavior:(_body, ordinal) => { seen.push(ordinal); return { text:'done' }; },
  });
  try {
    for (const [route, method] of [['models','GET'], ['messages/count_tokens','POST'], ['messages','POST']]) {
      for (const authorized of [false, true]) {
        const response = await fetch(`${upstream.url}/v1/${route}?beta=true`, {
          method, headers:{ 'x-api-key':authorized ? 'fixture-key' : 'wrong', 'content-type':'application/json' },
          ...(method === 'POST' ? { body:JSON.stringify({ model:'fixture-model', messages:[] }) } : {}),
        });
        assert.equal(response.status, authorized ? 200 : 401, `${route}/${authorized}`);
        const body = await response.json();
        if (authorized && route === 'models') assert.equal(body.data[0].id, 'fixture-model');
        if (authorized && route === 'messages/count_tokens') assert.equal(body.input_tokens, 10);
      }
      assert.deepEqual(seen, route === 'messages' ? [1] : []);
    }
    assert.equal(upstream.unauthorized, 3);
    assert.equal(upstream.attempts, 1);
  } finally { await upstream.close(); }
});

test('official SDK decodes the same callback tool arguments over JSON and SSE', async () => {
  // W7/W8: JSON/SSE x custom behavior -> exact tool id/arguments, including
  // Unicode and split escaped strings. W9: named behavior remains unchanged.
  // Decode with the official SDK, not another fixture-side SSE implementation.
  const input = { command:'echo "中文"\nnext' };
  const upstream = await startFakeAnthropic('fixture-key', {
    behavior:(_body, ordinal) => ({ tool:{ id:`tool-${ordinal}`, name:'Bash', input } }),
  });
  const client = new Anthropic({ baseURL:upstream.url, apiKey:'fixture-key', maxRetries:0 }); // awaken-allow: secret -- loopback-only synthetic fixture credential
  const params = { model:'fixture-model', max_tokens:32, messages:[{ role:'user', content:'fixture' }] };
  try {
    const replies = [await client.messages.create(params, { query:{ beta:'true' } }),
      await client.messages.stream(params, { query:{ beta:'true' } }).finalMessage()];
    for (const [index, reply] of replies.entries()) {
      assert.equal(reply.stop_reason, 'tool_use');
      assert.deepEqual(reply.content, [{ type:'tool_use', id:`tool-${index + 1}`, name:'Bash', input }]);
    }
  } finally { await upstream.close(); }
  const named = await startFakeAnthropic('fixture-key');
  try {
    const reply = await new Anthropic({ baseURL:named.url, apiKey:'fixture-key', maxRetries:0 }).messages.create(params); // awaken-allow: secret -- loopback-only synthetic fixture credential
    assert.equal(reply.content[0].text, 'FAKE:fixture');
  } finally { await named.close(); }
});
