// Public HTTPS qualification peer; reuses the canonical calc MCP fixture.
// Run on an isolated test host with a valid certificate and exact DNS origin.
import fs from 'node:fs';
import https from 'node:https';
import { randomBytes } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { startCalcFixture } from './mcp_calc_fixture.mjs';

const [originText, keyFile, certFile, bearerFile, observationTokenFile] = process.argv.slice(2);
const origin = new URL(originText);
if (origin.protocol !== 'https:' || origin.pathname !== '/' || origin.search || origin.hash
  || origin.username || origin.password || (origin.port && origin.port !== '443')) {
  throw new Error('qualification peer requires an exact public HTTPS origin on 443');
}
const sourceRevision = execFileSync('git', ['rev-parse','HEAD'], {encoding:'utf8'}).trim();
if (execFileSync('git',['status','--porcelain'],{encoding:'utf8'}).trim()) {
  throw new Error('qualification peer requires the exact clean Open source checkout');
}
const bearer = fs.readFileSync(bearerFile,'utf8').trim();
const observationToken = fs.readFileSync(observationTokenFile,'utf8').trim();
if (bearer.length < 32 || observationToken.length < 32 || bearer === observationToken) {
  throw new Error('peer requires separate high-entropy credential and observation token');
}
const opaqueResult = randomBytes(32).toString('hex');
const held = new Map();
let fixture;
const json = (res,status,value) => {res.writeHead(status, {'content-type':'application/json'});res.end(JSON.stringify(value));};
fixture = await startCalcFixture(bearer, {
  stateful:true, opaqueResult, listenHost:'0.0.0.0', listenPort:443,
  advertisedOrigin:origin.origin,
  serverFactory:(handler) => https.createServer({key:fs.readFileSync(keyFile),cert:fs.readFileSync(certFile)},handler),
  controlHandler:(req,res) => {
    const url = new URL(req.url,origin);
    if (!['/observe','/hold','/release'].includes(url.pathname)) return false;
    if (req.headers.authorization !== `Bearer ${observationToken}`) {json(res,401,{});return true;}
    const nonce = url.searchParams.get('nonce');
    if (!/^[0-9a-f]{32}$/.test(nonce ?? '')) {json(res,400,{});return true;}
    if (url.pathname === '/observe' && req.method === 'GET') {
      const calls = fixture.calls.filter((call) => call.nonce === nonce);
      json(res,200,{source_revision:sourceRevision, nonce, opaque_result:opaqueResult,
        calls:calls.map((call) => ({method:call.method,tool:call.tool,session_id:call.sessionId,
          authenticated:call.authorization === `Bearer ${bearer}`,completed:call.completed})),
        hold_started:held.get(nonce)?.started ?? false});
    } else if (url.pathname === '/hold' && req.method === 'POST') {
      if (held.has(nonce)) json(res,409,{});
      else {held.set(nonce,{started:false,release:null});json(res,204,null);}
    } else if (url.pathname === '/release' && req.method === 'POST') {
      held.get(nonce)?.release?.();held.delete(nonce);json(res,204,null);
    } else json(res,405,{});
    return true;
  },
  beforeToolCall:(_body,req,res) => {
    const hold = held.get(req.headers['x-awaken-probe-id']);
    if (!hold) return;
    hold.started = true;
    return new Promise((resolve,reject) => {
      hold.release = resolve;
      res.once('close',() => reject(new Error('caller disconnected')));
    });
  },
});
process.stdout.write(JSON.stringify({origin:origin.origin,source_revision:sourceRevision})+'\n');
for (const signal of ['SIGTERM','SIGINT']) process.on(signal,async () => {
  for (const hold of held.values()) hold.release?.();
  await fixture.close();process.exit(0);
});
