import assert from 'node:assert/strict';
import test from 'node:test';
import { loadQualifiedClients, qualifiedClient } from '../src/conformance/clients.mjs';
import { exerciseWorkspaceKeyFirstUse } from '../src/conformance/first-use.mjs';

const current = qualifiedClient(await loadQualifiedClients(), 'current_oracle');
for (const rule of ['typed-empty', 'html', 'missing-page', 'malformed-model-page', 'wrong-workspace', 'missing-known-fixture', 'missing-model-fixture', 'invalid-key', 'browser-base']) {
  test(`Workspace-bound official SDK first use: ${rule}`, async () => {
    // C1 correct service origin vs browser base; C2 real JSON page vs HTML or
    // malformed page; C3 correct response Workspace; C4 known fixture present;
    // C5 active/invalid credential. F1 C1&C2&C3&!known-fixture -> eight typed
    // flat SDK reads (both credential styles), no Workspace request selector;
    // F2 !C1 -> zero transport; F3 !C2/!C3/!C4 -> fail, never false empty PASS;
    // F4 invalid credential -> official AuthenticationError after one attempt.
    // Recording transport qualifies orchestration/SDK parsing only, not IAM.
    let requests = 0;
    const execute = () => exerciseWorkspaceKeyFirstUse({
      baseURL: rule === 'browser-base' ? 'https://agents.example.test/w/workspace' : 'https://agents.example.test',
      apiKey: 'inert-sdk-fixture', workspaceID: 'workspace', clients: [current], // awaken-allow(secret): inert recording-transport fixture, never issued by an authority.
      requireNonEmpty: rule === 'missing-known-fixture' ? ['agents'] : rule === 'missing-model-fixture' ? ['models'] : [],
      fetchImpl: async (input, init) => {
        requests++;
        const request = new Request(input, init);
        assert.equal(request.headers.get('anthropic-workspace-id'), null);
        const family = new URL(request.url).pathname.split('/').at(-1);
        const body = family === 'models'
          ? { data: [], has_more: false, first_id: null, last_id: null }
          : { data: [], next_page: null, ...(family === 'sessions' ? {prev_page: null} : {}) };
        const headers = { 'content-type': 'application/json', 'request-id': `fixture-${rule}-${requests}`,
          'anthropic-workspace-id': rule === 'wrong-workspace' ? 'other' : 'workspace' };
        if (rule === 'html') return new Response('<html></html>', { headers: {...headers, 'content-type':'text/html'} });
        if (rule === 'invalid-key') return new Response(JSON.stringify({type:'error',error:{type:'authentication_error',message:'invalid'}}), {status:401,headers});
        return new Response(JSON.stringify(rule === 'missing-page' || (rule === 'malformed-model-page' && family === 'models') ? {} : body), {headers});
      },
    });
    if (rule === 'typed-empty') { const rows = await execute(); assert.equal(rows.length, 2); assert.equal(requests, 8); }
    else { await assert.rejects(execute(), rule === 'invalid-key' ? {status:401} : undefined); assert.equal(requests, rule === 'browser-base' ? 0 : ['malformed-model-page', 'missing-model-fixture'].includes(rule) ? 3 : 1); }
  });
}
