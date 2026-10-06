import assert from 'node:assert/strict';
import test from 'node:test';
import {spawn} from 'node:child_process';
import {mkdtemp,readFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import {once} from 'node:events';
import {startCalcFixture} from './mcp_calc_fixture.mjs';

test('ACP qualification peer performs one stateful tool flow and rejects credential projection', {timeout:10000}, async()=>{
  // C1 real ACP child receives credential-free HTTP projection, C2 stateful
  // server, C3 exact prompt nonce -> E1 one call and opaque local proof.
  // R2 a projected credential header -> E2 ACP error before any new MCP I/O.
  // Anonymous upstream is a fixture-only transport check, not custody proof.
  const directory=await mkdtemp(path.join(tmpdir(),'g43-acp-fixture-'));
  const fixture=await startCalcFixture('synthetic-token',{stateful:true,allowAnonymous:true,opaqueResult:'server-only'});
  const child=spawn(process.execPath,[new URL('./acp_worker_mcp_fixture.mjs',import.meta.url).pathname,directory],
    {stdio:['pipe','pipe','pipe']});
  const lines=readline.createInterface({input:child.stdout});
  const pending=new Map();let id=1;let stderr='';
  child.stderr.on('data',chunk=>{stderr+=chunk;});
  lines.on('line',line=>{const value=JSON.parse(line);if(pending.has(value.id)) {
    pending.get(value.id)(value);pending.delete(value.id);
  }});
  const request=(method,params)=>new Promise(resolve=>{
    const requestId=id++;pending.set(requestId,resolve);
    child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:requestId,method,params})+'\n');
  });
  try {
    assert.equal((await request('initialize',{})).result.agentCapabilities.mcpCapabilities.http,true);
    await request('session/new',{mcpServers:[{name:'g43_mcp',url:fixture.url}]});
    const nonce='0123456789abcdef0123456789abcdef';
    assert.equal((await request('session/prompt',{prompt:[{text:`G43_MCP_CALL ${nonce}`}]})).result.stopReason,'end_turn');
    const proof=JSON.parse(await readFile(path.join(directory,`g43-${nonce}.json`),'utf8'));
    assert.equal(proof.nonce,nonce);assert.ok(proof.upstreamSessionId);
    assert.equal(proof.result.content[0].text,'server-only');
    assert.equal(fixture.calls.filter(call=>call.method==='tools/call').length,1);
    const count=fixture.calls.length;
    await request('session/load',{mcpServers:[{name:'g43_mcp',url:fixture.url,
      headers:[{name:'Authorization',value:'forbidden'}]}]});
    assert.equal((await request('session/prompt',{prompt:[{text:`G43_MCP_CALL ${nonce}`}]})).error.code,-32000);
    assert.equal(fixture.calls.length,count);assert.equal(stderr,'');
  } finally {
    const exited=once(child,'exit');child.kill('SIGTERM');await exited;
    lines.close();await fixture.close();await rm(directory,{recursive:true,force:true});
  }
});
