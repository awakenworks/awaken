// Deterministic peer for the pinned npm ACP adapters inside the sandbox image.
// The container-tier Claude and Codex rows launch the catalog's container argv
// (`claude-agent-acp`, `codex-acp`) through the image's `awaken-sandbox acp`
// bridge. This file stands in for those adapter packages only: the image,
// bridge, Worker placement, credential projection and Managed Session path stay
// production. It speaks the official ACP JSON-RPC wire and answers a prompt
// with the projected launch facts it observed, never with inference.
//
// Usage inside the image: node acp_container_adapter_fixture.mjs <claude|codex>
// Node builtins only: the image receives this one file.
import fs from 'node:fs';
import os from 'node:os';
import readline from 'node:readline';

const role = process.argv[2];
if (role !== 'claude' && role !== 'codex') {
  console.error(`unknown adapter role ${JSON.stringify(role)}`);
  process.exit(2);
}

const sessionId = `container-${role}-${process.pid}`;
const methods = [];
const send = (message) => process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
// Only a short prefix of a credential crosses back to the test; the full value
// stays in the sandbox.
const prefix = (value) => (typeof value === 'string' && value.length > 0 ? value.slice(0, 8) : 'absent');

function readJson(file) {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
}

// The facts each adapter row's catalog delivery promises, as this process sees
// them. Claude: process-secret Anthropic env. Codex: provider config in
// CODEX_CONFIG plus the API key only in the Session config home's auth.json.
function projectedFacts() {
  const home = process.env.HOME ?? '';
  const common = {
    role,
    uid: String(os.userInfo().uid),
    home,
    cwd: process.cwd(),
    methods: methods.join(','),
  };
  if (role === 'claude') {
    return {
      ...common,
      base: process.env.ANTHROPIC_BASE_URL ?? 'absent',
      model: process.env.ANTHROPIC_MODEL ?? 'absent',
      key: prefix(process.env.ANTHROPIC_API_KEY),
      config: process.env.CLAUDE_CONFIG_DIR ?? 'absent',
    };
  }
  const config = (() => {
    try {
      return JSON.parse(process.env.CODEX_CONFIG ?? 'null');
    } catch {
      return null;
    }
  })();
  const provider = config?.model_providers?.[config?.model_provider];
  return {
    ...common,
    base: provider?.base_url ?? 'absent',
    model: config?.model ?? 'absent',
    wire: provider?.wire_api ?? 'absent',
    key: prefix(readJson(`${home}/.codex/auth.json`)?.OPENAI_API_KEY),
    envkey: prefix(process.env.OPENAI_API_KEY),
  };
}

for await (const line of readline.createInterface({ input: process.stdin })) {
  if (line.trim() === '') continue;
  const request = JSON.parse(line);
  if (request.method === undefined) continue; // a client response
  methods.push(request.method);
  switch (request.method) {
    case 'initialize':
      send({
        id: request.id,
        result: {
          protocolVersion: 1,
          agentCapabilities: {
            loadSession: false,
            promptCapabilities: { embeddedContext: true },
            mcpCapabilities: { http: true },
          },
          // Codex's catalog row authenticates its capability probe with the
          // advertised `api-key` method; Claude advertises none.
          authMethods: role === 'codex' ? [{ id: 'api-key', name: 'API key' }] : [],
        },
      });
      break;
    case 'authenticate':
      send({ id: request.id, result: {} });
      break;
    case 'session/new':
      send({ id: request.id, result: { sessionId } });
      break;
    case 'session/prompt': {
      const text = (request.params?.prompt ?? []).map((block) => block.text ?? '').join('\n');
      const marker = /AWAKEN_CONTAINER_ADAPTER_[A-Za-z0-9_-]+/u.exec(text)?.[0] ?? 'absent';
      const facts = Object.entries({ marker, ...projectedFacts() })
        .map(([name, value]) => `${name}=${value}`)
        .join(' ');
      send({
        method: 'session/update',
        params: {
          sessionId,
          update: {
            sessionUpdate: 'agent_message_chunk',
            content: { type: 'text', text: `CONTAINER_ADAPTER ${facts}` },
          },
        },
      });
      send({ id: request.id, result: { stopReason: 'end_turn' } });
      break;
    }
    case 'session/cancel':
      break;
    default:
      if (request.id !== undefined) {
        send({ id: request.id, error: { code: -32601, message: `fixture has no ${request.method}` } });
      }
  }
}
