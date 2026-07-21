// A fake Anthropic-compatible upstream: POST …/messages answers the Messages
// shape over the REAL wire, so e2e drive the REAL provider path (provider-genai
// over HTTP) without a live key. It presents the Anthropic 401 envelope on a bad
// key, injects retryable faults on demand, and — the point of the `behavior`
// option — reproduces each deterministic scenario model's reply *on the wire*, so
// an e2e that used to boot an in-process stub model now runs the same scenario
// through GenaiExecutor + a real socket. `behavior` names the reproduced model
// (default = the historic `FAKE:<text>` echo + `use-tool:` round-trip).
//
// The behaviors read the Anthropic *wire* request (`system` top-level field,
// `messages` with user/assistant roles, tool results as `tool_result` blocks
// inside user messages, `tools` list), which is what GenaiExecutor emits from the
// neutral `ChatRequest` — so each behavior is a faithful port of the matching
// `awaken-server` model reading the neutral request.

import http from 'node:http';

const FIXTURE_PORT_FIRST = 30_000;
const FIXTURE_PORT_COUNT = 2_000;
let fixturePortCursor = (process.pid * 37) % FIXTURE_PORT_COUNT;

// Distinctive per-inference token usage the fake reports (Anthropic wire field names,
// incl. the prompt-cache breakdown that genai maps to cache_read/cache_creation), so a
// usage e2e can assert exact accumulated counts. Each model call reports these; an
// N-step turn accumulates N× them. Exported so tests import the expected values.
export const FAKE_USAGE = {
  input_tokens: 11,
  output_tokens: 7,
  cache_read_input_tokens: 3,
  cache_creation_input_tokens: 2,
};

// ---- wire accessors: read the Anthropic request the way the neutral model read
// the ChatRequest (system field, user text, tool-result count, images, tools). ----

function blockText(content) {
  if (typeof content === 'string') return content;
  return (content ?? []).filter((b) => b.type === 'text').map((b) => b.text).join('');
}

function systemText(parsed) {
  return blockText(parsed.system);
}

function userMessages(parsed) {
  // A tool result rides on a `user` message as a `tool_result` block; it is not a
  // user turn. A user turn is a user message carrying text (not just tool_result).
  return (parsed.messages ?? []).filter(
    (m) => m.role === 'user' && (typeof m.content === 'string' || (m.content ?? []).some((b) => b.type === 'text')),
  );
}

function lastUserText(parsed) {
  const users = userMessages(parsed);
  return users.length ? blockText(users[users.length - 1].content) : '';
}

function firstUserText(parsed) {
  const users = userMessages(parsed);
  return users.length ? blockText(users[0].content) : '';
}

function allUserText(parsed) {
  return userMessages(parsed).map((m) => blockText(m.content)).join(' ');
}

// Every `tool_result` block across the transcript — one per prior tool-role turn.
function toolResults(parsed) {
  const out = [];
  for (const m of parsed.messages ?? []) {
    if (!Array.isArray(m.content)) continue;
    for (const b of m.content) {
      if (b.type === 'tool_result') out.push(b);
    }
  }
  return out;
}

function toolResultText(block) {
  if (typeof block?.content === 'string') return block.content;
  return (block?.content ?? []).filter((b) => b.type === 'text').map((b) => b.text).join('');
}

function lastUserImages(parsed) {
  const users = (parsed.messages ?? []).filter((m) => m.role === 'user');
  const last = users[users.length - 1];
  if (!last || !Array.isArray(last.content)) return [];
  return last.content
    .filter((b) => b.type === 'image')
    .map((b) => (b.source?.type === 'url' ? 'image/url' : b.source?.media_type ?? 'image/*'));
}

function hasTool(parsed, name) {
  return (parsed.tools ?? []).some((t) => t.name === name);
}

// A reply is either `{ text }` (end_turn) or `{ tool: { id, name, input } }`
// (tool_use). The named behaviors below each port one `awaken-server` model.
const text = (t) => ({ text: t });
const tool = (id, name, input) => ({ tool: { id, name, input } });

export const BEHAVIORS = {
  // The historic default: echo the last user text, and drive one tool round-trip
  // on `use-tool:<name>` (kept so the fault-injection / real-wire e2e are unchanged).
  default(parsed) {
    const t = lastUserText(parsed);
    // The tool round-trip stays on the streaming path only (its historic home), so
    // a non-streamed `use-tool:` still echoes as text exactly as before.
    const m = parsed.stream && toolResults(parsed).length === 0 && /^use-tool:(\S+)/.exec(t);
    if (m) return tool(`toolu_${m[1]}`, m[1], { pattern: '*.md' });
    return text(`FAKE:${t}`);
  },
  // EchoModel: `Echo: <last user text>`.
  echo: (parsed) => text(`Echo: ${lastUserText(parsed)}`),
  // LabelModel(<label>): `model=<label>: <user>` — the label rides in via the model
  // name the caller set (ANTHROPIC_MODEL), which the wire echoes back as `parsed.model`.
  label: (parsed) => text(`model=${parsed.model ?? 'fake-model'}: ${lastUserText(parsed)}`),
  // InstructionEchoModel: `instructions: <system prompt>`.
  instruction: (parsed) => text(`instructions: ${systemText(parsed)}`),
  // ReviseModel: a draft, then FINAL once it sees the goal loop's feedback.
  revise: (parsed) => text(allUserText(parsed).includes('did not meet the goal') ? 'FINAL answer' : 'a rough draft'),
  // VisionProbeModel: report the media types on the last user turn.
  vision(parsed) {
    const medias = lastUserImages(parsed);
    const t = blockText(((parsed.messages ?? []).filter((m) => m.role === 'user').pop() ?? {}).content);
    return text(medias.length ? `saw ${medias.join(',')}; text: ${t}` : `saw no media; text: ${t}`);
  },
  // CompactionModel: the compactor sub-run returns a fixed summary; a main turn
  // prefixes its reply with the system/context text it received.
  compaction(parsed) {
    const sys = systemLines(parsed);
    if (allUserText(parsed).includes('summarize') || sys.includes('summar')) return text('SUMMARY: earlier turns folded');
    return text(`ctx:[${sys}] echo:${lastUserText(parsed)}`);
  },
  // MemoryProbeModel: the extractor sub-run saves one memory via `write_memory`
  // (named after a `fact-<tag>` token when present); a main turn prefixes its echo
  // with the system/context lines so recall injection is observable.
  memory(parsed) {
    const sys = systemLines(parsed);
    if (sys.includes('memory extraction Agent')) {
      if (toolResults(parsed).length > 0) return text('memory saved');
      const tag = allUserText(parsed).split(/\s+/).find((w) => w.startsWith('fact-'));
      const [name, content] = tag ? [tag, `remember ${tag}`] : ['sky-color', 'the sky is green today'];
      return tool('memwrite-1', 'write_memory', { name, kind: 'project', content });
    }
    return text(`recall:[${sys}] echo:${lastUserText(parsed)}`);
  },
  // MemoryResourceModel: write the user's text into the mounted store, then (once a
  // tool result is present) reply `memory persisted`.
  memoryResource(parsed) {
    if (toolResults(parsed).length > 0) return text('memory persisted');
    return tool('memres-1', 'write', { path: '.mnt/memory/note.md', content: lastUserText(parsed) });
  },
  // GitRepoModel: read the seed file, write a new file, then finish (sequenced off
  // the tool-result count).
  gitRepo(parsed) {
    switch (toolResults(parsed).length) {
      case 0: return tool('r', 'read', { path: 'workspace/repo/README.md' });
      case 1: return tool('w', 'write', { path: 'workspace/repo/NEW.txt', content: 'AGENT_REPO_MARKER_3390' });
      // The agent authors its OWN commit in the jail (ADR-0038: the host only pushes
      // what the agent committed — `push_repo_at` ships `@{u}..HEAD`). Inline identity
      // so a fresh host-side clone needs no prior git config.
      case 2: return tool('c', 'bash', { command: "cd workspace/repo && git add -A && git -c user.email=agent@awaken -c user.name=agent commit -m 'agent: add NEW.txt'" });
      default: return text('repo turn done');
    }
  },
  // StateMachineModel: call `glob` twice (walk s0->s1, then an out-of-order call the
  // gate rejects), then end.
  stateMachine(parsed) {
    switch (toolResults(parsed).length) {
      case 0: return tool('g1', 'glob', { pattern: '*.txt' });
      case 1: return tool('g2', 'glob', { pattern: '*.txt' });
      default: return text('done');
    }
  },
  // ProbeModel (HITL): write the user's text to probe.txt, read it back, then reply.
  probe(parsed) {
    switch (toolResults(parsed).length) {
      case 0: return tool('w', 'write', { path: 'probe.txt', content: firstUserText(parsed) });
      case 1: return tool('r', 'read', { path: 'probe.txt' });
      default: return text('done');
    }
  },
  // CustomToolModel: call the client-executed `submit_answer`, then reply with the
  // result the client returned.
  custom(parsed) {
    const results = toolResults(parsed);
    if (results.length === 0) return tool('c1', 'submit_answer', { question: 'what is 6 x 7?' });
    return text(`got: ${toolResultText(results[results.length - 1])}`);
  },
  // McpToolModel: `add <a> <b>` calls the namespaced MCP tool; a tool result is
  // reported as `result: <text>`; anything else echoes like EchoModel.
  mcp(parsed) {
    const results = toolResults(parsed);
    const msgs = parsed.messages ?? [];
    const last = msgs[msgs.length - 1];
    if (last && Array.isArray(last.content) && last.content.some((b) => b.type === 'tool_result')) {
      return text(`result: ${toolResultText(results[results.length - 1])}`);
    }
    const t = lastUserText(parsed);
    const p = t.split(/\s+/);
    if (p.length === 3 && p[0] === 'add' && /^-?\d+$/.test(p[1]) && /^-?\d+$/.test(p[2])) {
      return tool(`mcp-${msgs.length}`, 'mcp__calc__add', { a: Number(p[1]), b: Number(p[2]) });
    }
    return text(`Echo: ${t}`);
  },
  // FullChainModel (the native combined-chain e2e): one conversation that drives the
  // whole ADR-0038/0036 loop — discover+use a skill, write into the mounted memory
  // store, write into the cloned git repo, and produce an output artifact — plus its
  // out-of-band extractor sub-run that saves a memory. Sequenced by tool-result count
  // so it needs no transcript parsing; every write is host-gated and harvested.
  fullChain(parsed) {
    // The extraction sub-run (out-of-band): save one memory, then finish. It is seeded
    // with the whole main-turn transcript (which carries tool results), so we can't key
    // off a tool-result *count*; instead write_memory unless the LAST message is our
    // write_memory result (i.e. the tool just ran) — then finish.
    if (systemText(parsed).includes('memory extraction Agent')) {
      const msgs = parsed.messages ?? [];
      const last = msgs[msgs.length - 1];
      const lastIsToolResult = last && Array.isArray(last.content) && last.content.some((b) => b.type === 'tool_result');
      if (lastIsToolResult) return text('memory saved');
      return tool('mx', 'write_memory', {
        name: 'session-note',
        kind: 'project',
        content: 'remember: the full chain ran end to end',
      });
    }
    switch (toolResults(parsed).length) {
      case 0: return tool('ls', 'list_skills', {});
      case 1: return tool('sk', 'Skill', { skill: 'greet' });
      // Write into the mounted MemoryStore directory.
      case 2: return tool('wm', 'write', { path: '.mnt/memory/note.md', content: 'MEMO_FULLCHAIN_5521' });
      // Write into the cloned repo working tree (host commits + pushes on harvest).
      case 3: return tool('wr', 'write', { path: 'workspace/repo/CHAIN.txt', content: 'REPO_FULLCHAIN_8830' });
      // Produce an output artifact (harvested into the blob store, listed by /v1/files).
      case 4: return tool('wa', 'write', { path: 'outputs/result.txt', content: 'ARTIFACT_FULLCHAIN_9142' });
      // Commit the repo edit in the jail so the host push-back has something to ship.
      case 5: return tool('wc', 'bash', { command: "cd workspace/repo && git add -A && git -c user.email=agent@awaken -c user.name=agent commit -m 'agent: add CHAIN.txt'" });
      default: return text('done: used skill greet, wrote memory + repo + artifact');
    }
  },
  // SkillDrivingModel: on the user turn call `list_skills`; given the catalog
  // activate `greet` via the `Skill` tool; given the activation instructions reply
  // `USED-SKILL: <instructions>` — discover → activate → use.
  skills(parsed) {
    const msgs = parsed.messages ?? [];
    const last = msgs[msgs.length - 1];
    const results = last && Array.isArray(last.content) ? last.content.filter((b) => b.type === 'tool_result') : [];
    if (results.length === 0) return tool('l', 'list_skills', {});
    const lastText = results.map(toolResultText).join('');
    if (lastText.includes('"skills"')) return tool('s', 'Skill', { skill: 'greet' });
    return text(`USED-SKILL: ${lastText}`);
  },
  // AdminAssistantModel (ADR-0052): drive the seeded management assistant through all
  // five admin tools in one run, sequenced by tool-result count, then a
  // final marker. Every draft it passes is a valid ordinary config (auto-bound, no
  // tools) so the real DraftValidator accepts it.
  adminDrive(parsed) {
    switch (toolResults(parsed).length) {
      case 0: return tool('c0', 'admin_get_platform_capabilities', {});
      case 1: return tool('c1', 'admin_draft_agent', { id: 'drafted-agent', instructions: 'a drafted agent' });
      case 2: return tool('c2', 'admin_patch_agent', { id: 'drafted-agent', patch: { description: 'patched by the admin assistant' } });
      case 3: return tool('c3', 'admin_validate_agent', { id: 'drafted-agent' });
      case 4: return tool('c4', 'admin_explain_console', { topic: 'agent' });
      default: return text('ADMIN-RUN-DONE: capabilities read, draft created, patched, validated, help read');
    }
  },
  // DelegatingModel: with `agent_run` it delegates (to `researcher`, or `ghost` if
  // asked) and reports the delegate's result; without it, it answers plainly (so the
  // same behavior serves as the delegate sub-agent).
  delegating(parsed) {
    if (!hasTool(parsed, 'agent_run')) return text('researched: 42');
    const results = toolResults(parsed);
    if (results.length === 0) {
      const agentId = firstUserText(parsed).includes('ghost') ? 'ghost' : 'researcher';
      return tool('d1', 'agent_run', { agent_id: agentId, input: 'do the research' });
    }
    return text(`delegate said: ${toolResultText(results[results.length - 1])}`);
  },
};

// System messages arrive as the top-level `system` field; the compaction/memory
// models joined multiple System lines with " | ", but the wire concatenates them —
// so match on the concatenated text (a `contains` check, which is all they do).
function systemLines(parsed) {
  return systemText(parsed);
}

// `opts`: `{ behavior, failuresBeforeSuccess, alwaysFail, faultStatus, delayMs }`.
// `behavior` (default `'default'`) selects the reproduced scenario model; the
// fault-injection knobs drive the runtime's retry + circuit-breaker + error paths.
export function startFakeAnthropic(apiKey, opts = {}) {
  const {
    behavior = 'default',
    failuresBeforeSuccess = 0,
    alwaysFail = false,
    faultStatus = 503,
    delayMs = 0,
    failModel = null,
  } = opts;
  const reply_of = BEHAVIORS[behavior];
  if (!reply_of) throw new Error(`unknown fake-anthropic behavior: ${behavior}`);
  const state = { requests: [], unauthorized: 0, attempts: 0, received: 0 };
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', async () => {
      // Count arrival before an optional response delay. Crash/recovery e2e use
      // this externally observable socket fact to kill a worker while inference
      // is genuinely in flight, without peeking into the Rust process.
      state.received += 1;
      if (delayMs > 0) await new Promise((r) => setTimeout(r, delayMs));
      const presented = req.headers['x-api-key'] ?? (req.headers.authorization ?? '').replace(/^Bearer /, '');
      if (req.method !== 'POST' || !req.url.endsWith('/messages')) {
        res.writeHead(404, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'not_found_error', message: `no ${req.method} ${req.url}` } }));
        return;
      }
      if (presented !== apiKey) {
        state.unauthorized += 1;
        res.writeHead(401, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'authentication_error', message: 'invalid x-api-key' } }));
        return;
      }
      state.attempts += 1;
      const parsed = JSON.parse(body || '{}');
      // Per-model fault (#1): fail exactly the named model with a retryable
      // overloaded error, so a run fails over to its pool fallback. The attempt is
      // recorded (marked `failed`) so a test can see both models were tried in order.
      if (failModel && parsed.model === failModel) {
        state.requests.push({ url: req.url, model: parsed.model, stream: !!parsed.stream, failed: true });
        res.writeHead(faultStatus, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'overloaded_error', message: `model ${failModel} is down` } }));
        return;
      }
      if (alwaysFail || state.attempts <= failuresBeforeSuccess) {
        res.writeHead(faultStatus, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'overloaded_error', message: 'fake upstream is overloaded' } }));
        return;
      }
      state.requests.push({ url: req.url, model: parsed.model, stream: !!parsed.stream });
      const reply = reply_of(parsed);
      const id = `msg_${state.requests.length}`;
      const model = parsed.model ?? 'fake-model';

      if (parsed.stream) {
        emitStream(res, { id, model, reply });
        return;
      }
      emitJson(res, { id, model, reply });
    });
  });
  return new Promise((resolve, reject) => {
    let attempts = 0;
    const listen = () => {
      if (attempts >= FIXTURE_PORT_COUNT) {
        reject(new Error('no free fake-upstream port in the fixture range'));
        return;
      }
      const port = FIXTURE_PORT_FIRST + fixturePortCursor;
      fixturePortCursor = (fixturePortCursor + 1) % FIXTURE_PORT_COUNT;
      attempts += 1;
      const onError = (error) => {
        if (error?.code === 'EADDRINUSE') listen();
        else reject(error);
      };
      server.once('error', onError);
      server.listen(port, '127.0.0.1', () => {
        server.off('error', onError);
        const { port } = server.address();
        resolve({
          url: `http://127.0.0.1:${port}`,
          requests: state.requests,
          get unauthorized() {
            return state.unauthorized;
          },
          get attempts() {
            return state.attempts;
          },
          get received() {
            return state.received;
          },
          close: () => server.close(),
        });
      });
    };
    listen();
  });
}

// The Anthropic non-streaming response: one text or one tool_use content block.
function emitJson(res, { id, model, reply }) {
  const content = reply.tool
    ? [{ type: 'tool_use', id: reply.tool.id, name: reply.tool.name, input: reply.tool.input }]
    : [{ type: 'text', text: reply.text }];
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(
    JSON.stringify({
      id, type: 'message', role: 'assistant', model, content,
      stop_reason: reply.tool ? 'tool_use' : 'end_turn',
      stop_sequence: null,
      usage: { ...FAKE_USAGE },
    }),
  );
}

// Split `s` into up to `n` non-empty contiguous chunks, so a streamed tool call's
// arguments arrive as several `input_json_delta` events — exactly as Anthropic
// chunks `partial_json`. The server accumulates and parses at content_block_stop,
// so the assembled tool call is identical; only the live channel now carries true
// incremental argument deltas that a streaming tool-input protocol can paint.
function chunkString(s, n) {
  if (s.length <= 1) return [s];
  const size = Math.max(1, Math.ceil(s.length / n));
  const out = [];
  for (let i = 0; i < s.length; i += size) out.push(s.slice(i, i + size));
  return out;
}

// The Anthropic streaming wire: the fixed event ladder around one text or tool_use
// block, so GenaiExecutor's streaming path assembles the same response as `infer`.
function emitStream(res, { id, model, reply }) {
  res.writeHead(200, { 'content-type': 'text/event-stream' });
  const ev = (event, data) => res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  ev('message_start', {
    type: 'message_start',
    message: {
      id, type: 'message', role: 'assistant', model,
      content: [], stop_reason: null, stop_sequence: null,
      usage: {
        input_tokens: FAKE_USAGE.input_tokens,
        output_tokens: 0,
        cache_read_input_tokens: FAKE_USAGE.cache_read_input_tokens,
        cache_creation_input_tokens: FAKE_USAGE.cache_creation_input_tokens,
      },
    },
  });
  if (reply.tool) {
    ev('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: reply.tool.id, name: reply.tool.name, input: {} } });
    // Chunk the arguments across several `input_json_delta` events exactly as
    // Anthropic streams `partial_json`, so the live channel carries true
    // incremental tool-input deltas (not one whole-args frame). The server still
    // accumulates + parses at content_block_stop, so the assembled call is identical.
    for (const partial of chunkString(JSON.stringify(reply.tool.input), 4)) {
      ev('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: partial } });
    }
    ev('content_block_stop', { type: 'content_block_stop', index: 0 });
    ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'tool_use', stop_sequence: null }, usage: { output_tokens: FAKE_USAGE.output_tokens } });
  } else {
    ev('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } });
    // Chunk the assistant text across several `text_delta` events exactly as
    // Anthropic streams incremental text, so the live channel carries true
    // token-level deltas (not one whole-text frame). Every text consumer
    // concatenates the deltas, so the assembled message is identical.
    for (const part of chunkString(reply.text, 4)) {
      ev('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: part } });
    }
    ev('content_block_stop', { type: 'content_block_stop', index: 0 });
    ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: FAKE_USAGE.output_tokens } });
  }
  ev('message_stop', { type: 'message_stop' });
  res.end();
}
