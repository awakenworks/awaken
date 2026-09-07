import assert from 'node:assert/strict';
import test from 'node:test';
import {startCalcFixture} from './mcp_calc_fixture.mjs';

// Endpoint graph: explicit/default bind address -> advertised JSON-RPC and OAuth
// URLs use that same address -> caller reaches the actual listener. Auth remains
// independent: anonymous opt-in x missing/wrong/correct bearer -> 200/401.
// L1 default loopback; L2 explicit alternate loopback catches hardcoded 127.0.0.1;
// L3 rejected bind rejects the startup promise and creates no usable fixture.
for (const listenHost of [undefined, '127.0.0.2']) {
  test(`MCP listener honors ${listenHost ?? 'default'} without widening auth`, {timeout:5000}, async()=>{
    for (const allowAnonymous of [false,true]) {
      const fixture=await startCalcFixture('synthetic-mcp-token',{listenHost,allowAnonymous,opaqueResult:'fresh-fixture-result'});
      try {
        const url=new URL(fixture.url);
        assert.equal(url.hostname,listenHost ?? '127.0.0.1');
        assert.equal(fixture.tokenUrl,new URL('token',fixture.url).href);
        for (const bearer of [undefined,'wrong','synthetic-mcp-token']) {
          const response=await fetch(fixture.url,{method:'POST',headers:{'content-type':'application/json',
            ...(bearer ? {authorization:`Bearer ${bearer}`} : {})},
            body:JSON.stringify({jsonrpc:'2.0',id:1,method:'tools/call',params:{name:'attest',arguments:{}}})});
          const allowed=bearer==='synthetic-mcp-token' || (allowAnonymous && bearer===undefined);
          assert.equal(response.status,allowed ? 200 : 401);
          if (allowed) assert.equal((await response.json()).result.content[0].text,'fresh-fixture-result');
          else await response.text();
        }
      } finally {await fixture.close();}
    }
  });
}
test('unassigned MCP bind address rejects startup', {timeout:5000}, async()=>{
  let fixture;
  try {
    await assert.rejects(async()=>{fixture=await startCalcFixture('synthetic-token',{listenHost:'192.0.2.123'});},
      error=>error.code==='EADDRNOTAVAIL');
  } finally {await fixture?.close();}
});
