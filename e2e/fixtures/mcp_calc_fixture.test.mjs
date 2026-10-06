import assert from 'node:assert/strict';
import test from 'node:test';
import {startCalcFixture} from './mcp_calc_fixture.mjs';
import {mcpRequest, mcpNotify} from './mcp_http_fixture_client.mjs';

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

test('stateful MCP fixture requires one upstream session throughout the shared client flow', {timeout:5000}, async()=>{
  // C1 authorized stateful initialize, C2 captured session id, C3 DELETE.
  // R1 C1+C2 -> list/call; R2 !C2 -> 400 without a recorded call;
  // R3 C3 -> following use fails. Existing stateless fixture behavior remains
  // covered above; the shared client is the sole ACP fixture HTTP transport.
  const fixture = await startCalcFixture('synthetic-token',{stateful:true,opaqueResult:'opaque',
    controlHandler:(req,res)=>{
      if(req.url!=='/observe') return false;
      res.end('observer');return true;
    }});
  try {
    assert.equal(await (await fetch(new URL('/observe',fixture.url))).text(),'observer');
    const state={nextId:1,sessionId:null,headers:{authorization:'Bearer synthetic-token',
      'x-awaken-probe-id':'0123456789abcdef0123456789abcdef'}};
    await mcpRequest(fixture.url,'initialize',{protocolVersion:'2025-06-18'},state);
    assert.ok(state.sessionId);
    await mcpNotify(fixture.url,'notifications/initialized',state);
    assert.ok((await mcpRequest(fixture.url,'tools/list',{},state)).tools.length);
    assert.equal((await mcpRequest(fixture.url,'tools/call',{name:'attest',arguments:{}},state)).content[0].text,'opaque');
    const count=fixture.calls.length;
    const missing=await fetch(fixture.url,{method:'POST',headers:{authorization:'Bearer synthetic-token'},
      body:JSON.stringify({jsonrpc:'2.0',id:99,method:'tools/list'})});
    assert.equal(missing.status,400);await missing.text();
    assert.equal(fixture.calls.length,count);
    const deleted=await fetch(fixture.url,{method:'DELETE',headers:{...state.headers,'mcp-session-id':state.sessionId}});
    assert.equal(deleted.status,204);
    await assert.rejects(()=>mcpRequest(fixture.url,'tools/list',{},state),/returned 400/);
    assert.equal(fixture.calls.filter(call=>call.method==='tools/call').length,1);
    assert.ok(fixture.calls.every(call=>call.nonce===state.headers['x-awaken-probe-id']));
  } finally {await fixture.close();}
});
