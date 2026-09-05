// One deterministic provider shared by real SDK and real ACP adapter checks.
import http from 'node:http';
export async function startClaudeInference() {
  let turns = 0; const requests = [];
  const server = http.createServer(async (req, res) => {
    const chunks = []; for await (const chunk of req) chunks.push(chunk);
    let body = {}; try { body = JSON.parse(Buffer.concat(chunks).toString()); } catch {}
    if (!req.url.startsWith('/v1/messages')) { res.writeHead(200, { 'content-type':'application/json' }); res.end('{}'); return; }
    if (req.url.includes('count_tokens')) { res.writeHead(200, { 'content-type':'application/json' }); res.end('{"input_tokens":10}'); return; }
    turns++; requests.push(body);
    const invoke = turns === 1;
    const block = invoke ? { type:'tool_use', id:'toolu_alias_test', name:'Bash', input:{ command:'AWAKEN_ALIAS_SENTINEL' } } : { type:'text', text:'finished' };
    const message = { id:`msg_${turns}`, type:'message', role:'assistant', model:'claude-sonnet-4-6', content:[block], stop_reason:invoke?'tool_use':'end_turn', stop_sequence:null, usage:{ input_tokens:10, output_tokens:5 } };
    if (!body.stream) { res.writeHead(200, { 'content-type':'application/json' }); res.end(JSON.stringify(message)); return; }
    res.writeHead(200, { 'content-type':'text/event-stream', 'cache-control':'no-cache' });
    const emit = (type, value) => res.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...value })}\n\n`);
    emit('message_start', { message:{ ...message, content:[], stop_reason:null } });
    emit('content_block_start', { index:0, content_block:invoke?{ ...block,input:{} }:{ type:'text',text:'' } });
    emit('content_block_delta', { index:0, delta:invoke?{ type:'input_json_delta',partial_json:JSON.stringify(block.input) }:{ type:'text_delta',text:block.text } });
    emit('content_block_stop', { index:0 });
    emit('message_delta', { delta:{ stop_reason:message.stop_reason, stop_sequence:null }, usage:{ output_tokens:5 } });
    emit('message_stop', {}); res.end();
  });
  await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  return { base, requests, turns: () => turns, close: async () => { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}
