import assert from 'node:assert/strict';
import test from 'node:test';
import {spawn} from 'node:child_process';
import {mkdtemp,readFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import {fileURLToPath} from 'node:url';
import {startCalcFixture} from './mcp_calc_fixture.mjs';

function launchPeer(directory,{signal,script=fileURLToPath(new URL('./acp_worker_mcp_fixture.mjs',import.meta.url))}={}) {
  const child=spawn(process.execPath,[script,directory],{stdio:['pipe','pipe','pipe']});
  const lines=readline.createInterface({input:child.stdout});
  const pending=new Map();let id=1;let stderr='';let failure;let closed=false;let resolveClosed;let stopping;
  const closeObserved=new Promise(resolve=>{resolveClosed=resolve;});
  const fail=error=>{
    failure??=error;
    for(const waiter of pending.values()) waiter.reject(error);
    pending.clear();
  };
  child.once('error',fail);
  child.stdin.on('error',fail);
  child.stderr.on('data',chunk=>{stderr+=chunk;});
  child.once('close',(code,termination)=>{
    closed=true;fail(new Error(`ACP fixture exited (${code??termination})`));resolveClosed();
  });
  lines.on('line',line=>{
    try {
      const value=JSON.parse(line);const waiter=pending.get(value.id);
      if(waiter){pending.delete(value.id);waiter.resolve(value);}
    } catch(error){fail(error);}
  });
  const aborted=()=>{fail(new Error('ACP fixture aborted',{cause:signal.reason}));child.kill('SIGTERM');};
  signal?.addEventListener('abort',aborted,{once:true});
  if(signal?.aborted) aborted();
  const waitClosed=milliseconds=>new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>reject(new Error('ACP fixture did not close within its cleanup deadline')),milliseconds);
    closeObserved.then(()=>{clearTimeout(timer);resolve();});
  });
  return {
    stderr:()=>stderr,
    request:(method,params)=>{
      if(failure) return Promise.reject(failure);
      return new Promise((resolve,reject)=>{
        const requestId=id++;pending.set(requestId,{resolve,reject});
        try {child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:requestId,method,params})+'\n');}
        catch(error){fail(error);}
      });
    },
    stop:()=>stopping??=(async()=>{
      try {
        if(!closed){
          child.kill('SIGTERM');
          try {await waitClosed(2000);}
          catch {child.kill('SIGKILL');await waitClosed(2000);}
        }
      } finally {signal?.removeEventListener('abort',aborted);lines.close();}
    })(),
  };
}

test('ACP qualification peer performs one stateful tool flow and rejects credential projection', {timeout:10000}, async t=>{
  // C1 real ACP child receives credential-free HTTP projection, C2 stateful
  // server, C3 exact prompt nonce -> E1 one call and opaque local proof.
  // R2 a projected credential header -> E2 ACP error before any new MCP I/O.
  // Anonymous upstream is a fixture-only transport check, not custody proof.
  const directory=await mkdtemp(path.join(tmpdir(),'g43-acp-fixture-'));
  t.after(()=>rm(directory,{recursive:true,force:true}));
  const fixture=await startCalcFixture('synthetic-token',{stateful:true,allowAnonymous:true,opaqueResult:'server-only'});
  const peer=launchPeer(directory,{signal:t.signal});
  const request=peer.request;
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
    assert.equal(fixture.calls.length,count);assert.equal(peer.stderr(),'');
  } finally {
    try {await peer.stop();} finally {await fixture.close();}
  }
});

test('ACP peer early exit rejects pending and later requests without a lost exit event',{timeout:5000},async t=>{
  const peer=launchPeer(tmpdir(),{signal:t.signal,
    script:fileURLToPath(new URL('./missing-acp-peer.mjs',import.meta.url))});
  t.after(()=>peer.stop());
  await assert.rejects(peer.request('initialize',{}),/ACP fixture exited|EPIPE/);
  await assert.rejects(peer.request('initialize',{}),/ACP fixture exited|EPIPE/);
  await peer.stop();
  await peer.stop();
});

test('ACP peer cancellation rejects an unanswered request and confirms child cleanup',{timeout:5000},async t=>{
  const controller=new AbortController();
  const peer=launchPeer(tmpdir(),{signal:controller.signal});
  t.after(()=>peer.stop());
  const refused=assert.rejects(peer.request('never-replies',{}),/aborted/);
  controller.abort();
  await refused;
  await peer.stop();
});
