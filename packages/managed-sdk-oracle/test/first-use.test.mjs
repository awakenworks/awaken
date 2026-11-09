import assert from 'node:assert/strict';
import test from 'node:test';
import { loadQualifiedClients, qualifiedClient } from '../src/conformance/clients.mjs';
import { exerciseWorkspaceKeyFirstUse } from '../src/conformance/first-use.mjs';

const current = qualifiedClient(await loadQualifiedClients(), 'current_oracle');
for (const rule of ['typed-empty', 'typed-model-beta', 'missing-model-field', 'transport-error', 'html', 'missing-page', 'malformed-model-page', 'wrong-workspace', 'missing-known-fixture', 'missing-model-fixture', 'invalid-key', 'browser-base']) {
  test(`Workspace-bound official SDK first use: ${rule}`, async () => {
    // C1 correct service origin vs browser base; C2 real JSON page vs HTML or
    // malformed page; C3 correct response Workspace; C4 known fixture present;
    // C5 active/invalid credential. F1 C1&C2&C3&!known-fixture -> eight typed
    // flat SDK reads (both credential styles), no Workspace request selector;
    // F2 !C1 -> zero transport; F3 !C2/!C3/!C4 -> fail, never false empty PASS;
    // F4 invalid credential -> official AuthenticationError after one attempt.
    // Recording transport qualifies orchestration/SDK parsing only, not IAM.
    // Beta/diagnostic extension: C6 a nonempty Model is projected as Beta only
    // when the caller sends the Managed capability; C7 a required response
    // field is absent; C8 transport itself fails. F5 C6 -> explicitly negotiate
    // Beta through SDK parameters, preserving both auth styles and no Workspace
    // selector; F6 C7 -> original response AssertionError outside SDK fetch,
    // rather than a misleading APIConnectionError; F7 C8 -> genuine SDK
    // APIConnectionError. Empty pages alone cannot exercise C6/C7.
    let requests = 0;
    const execute = () => exerciseWorkspaceKeyFirstUse({
      baseURL: rule === 'browser-base' ? 'https://agents.example.test/w/workspace' : 'https://agents.example.test',
      apiKey: 'inert-sdk-fixture', workspaceID: 'workspace', clients: [current], // awaken-allow(secret): inert recording-transport fixture, never issued by an authority.
      requireNonEmpty: rule === 'missing-known-fixture' ? ['agents'] : ['missing-model-fixture', 'typed-model-beta'].includes(rule) ? ['models'] : [],
      fetchImpl: async (input, init) => {
        requests++;
        const request = new Request(input, init);
        assert.equal(request.headers.get('anthropic-workspace-id'), null);
        const family = new URL(request.url).pathname.split('/').at(-1);
        const body = family === 'models'
          ? { data: [], has_more: false, first_id: null, last_id: null }
          : { data: [], next_page: null, ...(family === 'sessions' ? {prev_page: null} : {}) };
        if (rule === 'transport-error') throw new TypeError('inert transport failure');
        if (family === 'models' && ['typed-model-beta', 'missing-model-field'].includes(rule)) {
          assert.ok(request.headers.get('anthropic-beta')?.split(',').includes('managed-agents-2026-04-01'), 'F5 explicit Beta selection, independent of automatic SDK query qualifiers');
          body.data = [{id:'model-fixture',type:'model',display_name:'Model fixture',created_at:'2026-01-01T00:00:00Z',
            allowed_fallback_models:[],capabilities:null,deprecated_at:null,retires_at:null,line:null,lifecycle:'active',max_input_tokens:null,max_tokens:null}];
          body.first_id = body.last_id = 'model-fixture';
          if (rule === 'missing-model-field') delete body.data[0].display_name;
        }
        const headers = { 'content-type': 'application/json', 'request-id': `fixture-${rule}-${requests}`,
          'anthropic-workspace-id': rule === 'wrong-workspace' ? 'other' : 'workspace' };
        if (rule === 'html') return new Response('<html></html>', { headers: {...headers, 'content-type':'text/html'} });
        if (rule === 'invalid-key') return new Response(JSON.stringify({type:'error',error:{type:'authentication_error',message:'invalid'}}), {status:401,headers});
        return new Response(JSON.stringify(rule === 'missing-page' || (rule === 'malformed-model-page' && family === 'models') ? {} : body), {headers});
      },
    });
    if (['typed-empty', 'typed-model-beta'].includes(rule)) { const rows = await execute(); assert.equal(rows.length, 2); assert.equal(requests, 8); }
    else {
      await assert.rejects(execute(), rule === 'invalid-key' ? {status:401}
        : rule === 'missing-model-field' ? {name:'AssertionError',code:'ERR_ASSERTION'}
        : rule === 'transport-error' ? (error) => error instanceof current.Client.APIConnectionError : undefined);
      assert.equal(requests, rule === 'browser-base' ? 0 : ['malformed-model-page', 'missing-model-fixture', 'missing-model-field'].includes(rule) ? 3 : 1);
    }
  });
}
