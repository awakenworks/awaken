// Deterministic external ACP peer. Transport configuration is never projected
// into model tape. Secrets used by application login never enter this process.
import {readFile,writeFile} from 'node:fs/promises';
import {createInterface} from 'node:readline';
const path=process.argv[2],request=JSON.parse(await readFile(path,'utf8'));
let server,phase='startup';const pending=new Map();
const send=value=>process.stdout.write(JSON.stringify(value)+'\n');
const lines=createInterface({input:process.stdin});
lines.on('line',async line=>{try{
 const rpc=JSON.parse(line),answer=result=>send({jsonrpc:'2.0',id:rpc.id,result});
 phase=rpc.method||'permission-result';
 if(!rpc.method&&pending.has(rpc.id)){pending.get(rpc.id)(rpc.result);pending.delete(rpc.id);return;}
 if(rpc.method==='initialize')answer({protocolVersion:1,agentCapabilities:{loadSession:true,mcpCapabilities:{http:true}}});
 else if(rpc.method==='session/new'||rpc.method==='session/load'){
   server=rpc.params.mcpServers.find(s=>s.name==='application');
   answer({sessionId:'qualification-acp'});
 }else if(rpc.method==='session/prompt'){
   const permission=new Promise(resolve=>pending.set('permission',resolve));
   send({jsonrpc:'2.0',id:'permission',method:'session/request_permission',params:{sessionId:'qualification-acp',toolCall:{toolCallId:'login-operation',title:'mcp.application.'+request.tool,rawInput:request.arguments},options:[{optionId:'once',name:'Allow',kind:'allow_once'},{optionId:'reject',name:'Reject',kind:'reject_once'}]}});
   if((await permission)?.outcome?.optionId!=='once')throw Error('permission refused');
   const headers=Object.fromEntries(server.headers.map(h=>[h.name,h.value]));
   const initialized=await fetch(server.url,{method:'POST',headers:{...headers,'Content-Type':'application/json'},body:JSON.stringify({jsonrpc:'2.0',id:2,method:'initialize',params:{protocolVersion:'2025-03-26',capabilities:{},clientInfo:{name:'qualification-acp',version:'1'}}}),signal:AbortSignal.timeout(20000)});
   if(!initialized.ok||(await initialized.json()).error)throw Error('MCP initialize refused');
   const call={jsonrpc:'2.0',id:1,method:'tools/call',params:{name:request.tool,arguments:request.arguments}};
   const response=await fetch(server.url,{method:'POST',headers:{...headers,'Content-Type':'application/json'},body:JSON.stringify(call),signal:AbortSignal.timeout(20000)});
   const result=await response.json();
   const proof={result,context:Object.fromEntries(Object.entries(headers).filter(([k])=>k.startsWith('x-awaken-'))),model_tape:{prompt:rpc.params.prompt,request:call,result}};
   await writeFile(path+'.result.json',JSON.stringify(proof));
   send({jsonrpc:'2.0',method:'session/update',params:{sessionId:'qualification-acp',update:{sessionUpdate:'agent_message_chunk',content:{type:'text',text:JSON.stringify(result)}}}});
   answer({stopReason:'end_turn'});
   lines.close();process.stdin.destroy();
 }
}catch(error){await writeFile(path+'.error.json',JSON.stringify({phase,message:String(error.message),cause:error.cause?.message,code:error.cause?.code}));process.exitCode=1;lines.close();process.stdin.destroy();}});
