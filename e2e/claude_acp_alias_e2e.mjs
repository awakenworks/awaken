// Actual pinned ACP adapter -> its actual SDK -> stdio MCP, with loopback inference.
// R1 alias-enabled model emits Bash -> canonical MCP called once; R2 permission
// title remains observable and no target/schema rewrite is inferred. Protocol
// tests own new/load/recovery metadata permutations. No real account is required.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { startClaudeInference } from './fixtures/claude_inference_fixture.mjs';
const root = await fs.mkdtemp(path.join(os.tmpdir(),'awaken-real-acp-alias-'));
const provider = await startClaudeInference();
const record = path.join(root,'calls.jsonl');
const fixture = fileURLToPath(new URL('./fixtures/claude_alias_mcp_fixture.mjs',import.meta.url));
const executable = fileURLToPath(new URL('./node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js',import.meta.url));
const child = spawn(process.execPath,[executable],{cwd:root,detached:true,stdio:['pipe','pipe','pipe'],env:{...process.env,HOME:root,CLAUDE_CONFIG_DIR:root,ANTHROPIC_API_KEY:'fixture-key',ANTHROPIC_AUTH_TOKEN:'',CLAUDE_CODE_OAUTH_TOKEN:'',ANTHROPIC_BASE_URL:provider.base,ANTHROPIC_MODEL:'claude-sonnet-4-6',CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:'1'}}); // awaken-allow: secret -- loopback-only fixture key, no provider account
let stderr=''; child.stderr.on('data',chunk => {stderr+=chunk});
const pending = new Map(); let nextId=1; const permissionTitles=[];
const send = (message) => child.stdin.write(JSON.stringify(message)+'\n');
readline.createInterface({input:child.stdout}).on('line',line => {
  const frame=JSON.parse(line);
  if(frame.method==='session/request_permission') {
    permissionTitles.push(frame.params.toolCall.title);
    const option=frame.params.options.find(option => option.kind==='allow_once');
    send({jsonrpc:'2.0',id:frame.id,result:{outcome:{outcome:'selected',optionId:option.optionId}}});
  } else if(pending.has(frame.id)) {
    const {resolve,reject,timer}=pending.get(frame.id); clearTimeout(timer); pending.delete(frame.id);
    if(frame.error) reject(new Error(JSON.stringify(frame.error))); else resolve(frame.result);
  }
});
function rpc(method,params) {const id=nextId++;return new Promise((resolve,reject) => {const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`${method} timed out: ${stderr.slice(-2000)}`));},60000);pending.set(id,{resolve,reject,timer});send({jsonrpc:'2.0',id,method,params});});}
try {
  await rpc('initialize',{protocolVersion:1,clientCapabilities:{}});
  const session=await rpc('session/new',{cwd:root,mcpServers:[{name:'awaken_session',command:process.execPath,args:[fixture,record],env:[]}],_meta:{claudeCode:{options:{toolAliases:{Bash:'mcp__awaken_session__bash'},tools:[],disallowedTools:['Bash'],settingSources:[]}}}});
  const result=await rpc('session/prompt',{sessionId:session.sessionId,prompt:[{type:'text',text:'Invoke Bash once.'}]});
  assert.equal(result.stopReason,'end_turn');
  assert.equal(await fs.readFile(record,'utf8'),JSON.stringify({command:'AWAKEN_ALIAS_SENTINEL'})+'\n');
  assert.ok(provider.turns()>=2);
  assert.ok(permissionTitles.length>0,'actual adapter requests permission');
  console.log('PASS actual Claude ACP 0.69.0 -> SDK 0.3.232 -> MCP exactly once; permissions='+JSON.stringify(permissionTitles));
} finally {
  for(const {timer} of pending.values()) clearTimeout(timer);
  try {process.kill(-child.pid,'SIGTERM');} catch {}
  await provider.close(); await fs.rm(root,{recursive:true,force:true});
}
