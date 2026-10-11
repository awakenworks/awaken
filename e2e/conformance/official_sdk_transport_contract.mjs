import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

/** Exercise changed transport behavior on the exact qualified SDK root. */
export async function exerciseNonReplayableTransport({ Anthropic, packageRoot }) {
  // T1-T4: reusable JSON / one-shot stream x HTTP 503 / connection loss.
  // Reusable inputs retry once; streams never replay an unknown consumed body.
  // Zero Retry-After makes the observation deterministic without bypassing the
  // SDK retry owner. The fixture records actual fetches, not internal options.
  for (const stream of [false, true]) for (const failure of ['http', 'connection']) {
    let requests = 0;
    const client = new Anthropic({
      apiKey: 'transport-contract-fixture', // awaken-allow(secret): (synthetic fixture)
      baseURL: 'https://transport.invalid', maxRetries: 1, logLevel: 'off',
      fetch: async () => {
        requests++;
        if (failure === 'connection') throw new Error('injected connection loss');
        return new Response(JSON.stringify({type:'error',error:{type:'api_error',message:'retry probe'}}),
          {status:503,headers:{'content-type':'application/json','retry-after':'0'}});
      },
    });
    const body = stream
      ? new ReadableStream({start(controller) { controller.enqueue(new Uint8Array([1,2,3])); controller.close(); }})
      : {probe:true};
    await assert.rejects(client.post('/v1/transport-probe', {body}).asResponse());
    assert.equal(requests, stream ? 1 : 2, `T1-T4 stream=${stream} failure=${failure}`);
  }

  // T5-T7: absent/existing/multiple beta values x one OAuth provider. Existing
  // selectors remain intact and OAuth is present exactly once on the wire.
  const { OAUTH_API_BETA_HEADER } = await import(pathToFileURL(resolve(packageRoot, 'lib/credentials/types.mjs')));
  for (const initial of [undefined, OAUTH_API_BETA_HEADER, 'first-beta,second-beta']) {
    let observed;
    const client = new Anthropic({
      apiKey: null, baseURL:'https://transport.invalid', maxRetries:0,
      credentials:async()=>({token:'oauth-contract-fixture',expiresAt:null}), // awaken-allow(secret): (synthetic fixture)
      fetch:async (input, init)=> {
        observed=new Headers(init?.headers ?? input.headers);
        return new Response(JSON.stringify({data:[],has_more:false,first_id:null,last_id:null}),
          {headers:{'content-type':'application/json'}});
      },
    });
    await client.models.list({}, {headers:initial ? {'anthropic-beta':initial} : {}});
    assert.deepEqual(observed.get('anthropic-beta').split(',').map(v=>v.trim()),
      [...new Set([...(initial?.split(',') ?? []),OAUTH_API_BETA_HEADER])]);
    assert.equal(observed.get('authorization'),'Bearer oauth-contract-fixture');
    assert.equal(observed.get('x-api-key'),null);
  }
  // T8 disabled debug must not format headers; enabled debug must redact them.
  const { loggerFor, debugLogRequestDetails } = await import(pathToFileURL(resolve(packageRoot,'internal/utils/log.mjs')));
  let formatted=0;
  const lazy={get headers(){formatted++;return {'authorization':'private-fixture'};}};
  debugLogRequestDetails(loggerFor({logLevel:'off'}),'probe',lazy);
  assert.equal(formatted,0,'T8 disabled debug does no formatting work');
  let logged;
  const logger={debug(_message,value){logged=value;},info(){},warn(){},error(){}};
  debugLogRequestDetails(loggerFor({logger,logLevel:'debug'}),'probe',{headers:{authorization:'private-fixture'}});
  assert.equal(logged.headers.authorization,'***','T8 enabled debug redacts');
}

export async function exerciseRootSchemaReferences(packageRoot) {
  // S1 root ref plus sibling definitions must retain transformed definitions;
  // S2 nested refs remain exact; S3 caller input never mutates. Both locations
  // pass through the real SDK converter rather than a replica of its algorithm.
  const { transformJSONSchema } = await import(pathToFileURL(resolve(packageRoot,'lib/transform-json-schema.mjs')));
  const schema = {$ref:'#/$defs/item',$defs:{item:{type:'object',properties:{name:{type:'string'}},required:['name']}}};
  const before=structuredClone(schema);
  const converted=transformJSONSchema(schema);
  assert.equal(converted.$ref,schema.$ref);
  assert.ok(converted.$defs?.item, 'S1 definitions survive root ref');
  assert.equal(converted.$defs.item.type,'object');
  assert.deepEqual(schema,before);
  const nested=transformJSONSchema({type:'object',properties:{item:schema},required:['item']});
  assert.equal(nested.properties.item.$ref,schema.$ref);
  assert.deepEqual(nested.properties.item.$defs,converted.$defs);
}
