// Business-system substitutes only. The model and Awaken service are real.
// MCP framing/transport is owned by the official SDK, never copied from calc.
import http from 'node:http';
import { createHash, randomBytes } from 'node:crypto';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';
import { z } from 'zod';
import { closeHttpServer } from '../http_server.mjs';

const WRITE = new Set(['create_page', 'create_issue', 'create_task', 'post_message', 'add_note']);
export const isTemplateWrite = (name) => WRITE.has(name);

export async function startTemplateMcp(scenario, options = {}) {
  const token = randomBytes(24).toString('hex');
  const accounts = options.accounts ?? { [token]: "default" };
  const state = { authentications: [], calls: [], effects: [], replays: 0, unauthorized: 0, lostResponses: 0 };
  const byKey = new Map();
  const transports = new Set();
  const server = http.createServer(async (req, res) => {
    const account = accounts[req.headers.authorization?.replace(/^Bearer /, "") ?? ""];
    if (!account) {
      state.unauthorized++;
      res.writeHead(401, { 'www-authenticate': 'Bearer', 'content-type': 'application/json' });
      res.end(JSON.stringify({ error: 'unauthorized' }));
      return;
    }
    state.authentications.push(account);
    const connector = req.url?.split('?')[0]?.split('/').at(-1);
    const names = scenario.tools[connector];
    if (!names) { res.writeHead(404).end(); return; }
    const mcp = new McpServer({ name: connector, version: '1.0.0' });
    for (const name of names) {
      const write = WRITE.has(name);
      const inputSchema = write
        ? { external_id: z.string().min(1), title: z.string().min(1), body: z.string().min(1), source_id: z.string().optional(), due_date: z.string().optional() }
        : { id: z.string().optional(), query: z.string().optional() };
      mcp.registerTool(name, {
        description: write
          ? `Write one ${connector} record. external_id is the caller's stable idempotency key. Returns its real id and URL; read the receipt before linking it elsewhere.`
          : `Read ${connector} source data. Search/list operations return an index; use the corresponding read operation for full evidence.`,
        inputSchema,
        annotations: { readOnlyHint: !write, destructiveHint: false, idempotentHint: write },
      }, async (input) => {
        state.calls.push({ connector, name, account, input: structuredClone(input) });
        let value;
        if (write) {
          const key = `${connector}:${input.external_id}`;
          const digest = createHash('sha256').update(JSON.stringify(input)).digest('hex');
          const previous = byKey.get(key);
          if (previous && previous.digest !== digest) {
            return { isError: true, content: [{ type: 'text', text: 'idempotency_conflict: external_id already names a different payload' }] };
          }
          if (previous) { state.replays++; value = previous.value; }
          else {
            const id = `${connector}-${randomBytes(5).toString('hex')}`;
            value = { id, url: `https://${connector}.fixture.invalid/records/${id}`, ...input };
            byKey.set(key, { digest, value });
            state.effects.push({ connector, name, ...value });
            // Fault rule M4: remote commit precedes a lost response. The caller
            // must reconcile using the same operation key, never invent success.
            if (options.loseResponseAfterWrite && state.lostResponses === 0) {
              state.lostResponses++;
              res.destroy();
            }
          }
        } else {
          const records = scenario.records[connector] ?? [];
          if (name === 'search') value = records.map(({ id, title, url }) => ({ id, title, url }));
          else if (input.id) value = records.find((record) => record.id === input.id);
          else value = records.length === 1 ? records[0] : records;
          if (value === undefined) return { isError: true, content: [{ type: 'text', text: 'record_not_found' }] };
        }
        if (options.accounts && !write) value = { result: value, credential_account: account };
        return { content: [{ type: 'text', text: JSON.stringify(value) }] };
      });
    }
    const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined, enableJsonResponse: true });
    transports.add(transport);
    res.on('close', () => { transports.delete(transport); void transport.close(); void mcp.close(); });
    try { await mcp.connect(transport); await transport.handleRequest(req, res); }
    catch { if (!res.headersSent && !res.destroyed) res.writeHead(500).end(); }
  });
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  const base = `http://127.0.0.1:${server.address().port}`;
  return {
    token, state,
    servers: Object.keys(scenario.tools).map((name) => ({ type: 'url', name, url: `${base}/mcp/${name}` })),
    close: async () => { await Promise.all([...transports].map((t) => t.close())); await closeHttpServer(server); },
  };
}
