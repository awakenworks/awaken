import assert from 'node:assert/strict';
import test from 'node:test';
import Anthropic from '@anthropic-ai/sdk';
import { startFakeAnthropic } from './fake_anthropic_fixture.mjs';

test('held inference observes cancellation without blocking another request', { timeout:10_000 }, async () => {
  // W12/W13: JSON/SSE x held reply + another request -> independent completion
  // and original request ordinal. W14/W15: cancel held JSON/SSE -> callback
  // observes connection closure; its late tool reply is never emitted. A fresh
  // request still succeeds. W16: rejected behavior -> bounded wire 500, no
  // unhandled rejection. These are socket facts, not Runtime cancellation proof.
  const held = [];
  const upstream = await startFakeAnthropic('fixture-key', {
    behavior:async (body, ordinal, signal) => {
      if (body.model === 'reject') throw new Error('synthetic failure');
      if (body.model === 'hold') {
        let release;
        const gate = new Promise(resolve => { release = resolve; });
        const closed = new Promise(resolve => signal.addEventListener('abort', resolve, { once:true }));
        held.push({ release, closed });
        await Promise.race([gate, closed]);
      }
      return { text:`reply-${ordinal}` };
    },
  });
  const client = new Anthropic({ baseURL:upstream.url, apiKey:'fixture-key', maxRetries:0 }); // awaken-allow: secret -- synthetic local test credential
  const params = { max_tokens:32, messages:[{ role:'user', content:'fixture' }] };
  let ordinal = 0;
  const controllers = [];
  try {
    for (const stream of [false, true]) {
      for (const cancel of [false, true]) {
        const controller = new AbortController(); controllers.push(controller);
        const index = held.length;
        const response = stream
          ? client.messages.stream({ ...params, model:'hold' }, { signal:controller.signal }).finalMessage()
          : client.messages.create({ ...params, model:'hold' }, { signal:controller.signal });
        const outcome = Promise.resolve(response).then(value => ({ value }), error => ({ error }));
        const deadline = Date.now() + 2_000;
        while (held.length === index && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 5));
        assert.equal(held.length, index + 1, 'inference reached its barrier');
        const original = ++ordinal;
        const other = await client.messages.create({ ...params, model:'ready' });
        assert.equal(other.id, `msg_${++ordinal}`);
        if (cancel) {
          controller.abort(); await held[index].closed;
          assert.ok((await outcome).error);
        } else {
          held[index].release();
          const { value } = await outcome;
          assert.equal(value.id, `msg_${original}`);
          assert.equal(value.content[0].text, `reply-${original}`);
        }
      }
    }
    await assert.rejects(client.messages.create({ ...params, model:'reject' }), error => error.status === 500);
    assert.equal((await client.messages.create({ ...params, model:'ready' })).stop_reason, 'end_turn');
  } finally {
    for (const controller of controllers) controller.abort();
    for (const item of held) item.release();
    await upstream.close();
  }
});

test('explicit bind address serves the same authenticated model wire', async () => {
  // W10 default address -> loopback URL; W11 explicit reachable bind address ->
  // exact advertised host and unchanged discovery/inference/authentication. The
  // real Docker Cowork rows supply the live bridge gateway to this same option.
  for (const listenHost of [undefined, process.platform === 'linux' ? '127.0.0.2' : '127.0.0.1']) {
    const upstream = await startFakeAnthropic('fixture-key', { listenHost, models:['fixture-model'] });
    try {
      assert.equal(new URL(upstream.url).hostname, listenHost ?? '127.0.0.1');
      const response = await fetch(`${upstream.url}/v1/models`, {headers:{'x-api-key':'fixture-key'}}); // awaken-allow: secret -- synthetic local test credential
      assert.equal(response.status, 200);
      assert.equal((await response.json()).data[0].id, 'fixture-model');
      assert.equal((await fetch(`${upstream.url}/v1/models`)).status, 401);
    } finally { await upstream.close(); }
  }
});

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
