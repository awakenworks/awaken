// A fake Anthropic-compatible upstream: POST …/messages answers the Messages
// shape over the REAL wire, so e2e drive the REAL provider path (provider-genai
// over HTTP) without a live key. It presents the Anthropic 401 envelope on a bad
// key, injects retryable faults on demand, and — the point of the `behavior`
// option — reproduces each deterministic scenario model's reply *on the wire*, so
// an e2e that used to boot an in-process stub model now runs the same scenario
// through the selected provider executor + a real socket. `behavior` names the reproduced model
// (default = the historic `FAKE:<text>` echo + `use-tool:` round-trip).
//
// The behaviors read the Anthropic *wire* request (`system` top-level field,
// `messages` with user/assistant roles, tool results as `tool_result` blocks
// inside user messages, `tools` list), which is what the Anthropic Messages ACL emits from the
// neutral `ChatRequest` — so each behavior is a faithful port of the matching
// `awaken-server` model reading the neutral request.

import http from 'node:http';
import { closeHttpServer } from '../http_server.mjs';

const FIXTURE_PORT_FIRST = 30_000;
const FIXTURE_PORT_COUNT = 2_000;
let fixturePortCursor = (process.pid * 37) % FIXTURE_PORT_COUNT;

// Distinctive per-inference token usage the fake reports (Anthropic wire field names,
// incl. the prompt-cache breakdown that genai maps to cache_read/cache_creation), so a
// usage e2e can assert exact accumulated counts. Each model call reports these; an
// N-Step Run accumulates N× them. Exported so tests import the expected values.
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
  // User message. A User message carries text (not just tool_result).
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

function outcomeJudgeInput(parsed) {
  const prompt = lastUserText(parsed);
  if (!prompt.startsWith('Evaluate this Outcome input')) return null;
  const payload = prompt.slice(prompt.indexOf('\n') + 1);
  return JSON.parse(payload);
}

function evaluatedOutcomeText(input) {
  // `transcript` is the materialized frozen selection. `message_start/end` retain
  // global Thread positions for evidence/reporting and must not be applied a
  // second time to this already-sliced array.
  return (input.transcript ?? [])
    .map((message) => blockText(message.content))
    .join('\n');
}

// Every `tool_result` block across the transcript — one per prior tool-result message.
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

// Tool results after the most recent text-bearing User message. Managed
// coordination is multi-Step and Sessions contain multiple Runs, so counting
// the entire transcript would let an earlier Run skip roster discovery in a
// later one. This mirrors the scenario-host model's current-Run boundary.
function currentRunToolResults(parsed) {
  const messages = parsed.messages ?? [];
  let boundary = -1;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (
      message.role === 'user'
      && (typeof message.content === 'string' || (message.content ?? []).some((block) => block.type === 'text'))
    ) {
      boundary = index;
      break;
    }
  }
  const out = [];
  for (const message of messages.slice(boundary + 1)) {
    if (!Array.isArray(message.content)) continue;
    for (const block of message.content) {
      if (block.type === 'tool_result') out.push(block);
    }
  }
  return out;
}

function toolResultText(block) {
  if (typeof block?.content === 'string') return block.content;
  return (block?.content ?? []).filter((b) => b.type === 'text').map((b) => b.text).join('');
}

function latestCoordinatedThreadId(parsed) {
  for (const result of toolResults(parsed).reverse()) {
    try {
      const receipt = JSON.parse(toolResultText(result));
      if (receipt?.accepted === true && typeof receipt.session_thread_id === 'string') {
        return receipt.session_thread_id;
      }
    } catch {
      // An unrelated tool result is not a coordination receipt.
    }
  }
  return null;
}

function catalogSkillId(block) {
  const payload = JSON.parse(toolResultText(block));
  const id = payload?.skills?.[0]?.id;
  if (typeof id !== 'string' || id.length === 0) {
    throw new Error('list_skills returned no canonical skill id');
  }
  return id;
}

function lastUserImages(parsed) {
  const users = (parsed.messages ?? []).filter((m) => m.role === 'user');
  const last = users[users.length - 1];
  if (!last || !Array.isArray(last.content)) return [];
  return last.content
    .filter((b) => b.type === 'image')
    .map((b) => (b.source?.type === 'url' ? 'image/url' : b.source?.media_type ?? 'image/*'));
}

// Return only structural content evidence. E2Es need to prove that logical
// Files ids were materialized before provider I/O, but retaining base64 payloads
// in the fixture would create a second, potentially sensitive transcript.
function contentShape(content) {
  if (!Array.isArray(content)) return [];
  const shapes = [];
  for (const block of content) {
    if (block.type === 'tool_result') {
      shapes.push({
        type: 'tool_result',
        content: contentShape(block.content),
        content_container: Array.isArray(block.content) ? 'array' : typeof block.content,
        content_length: Array.isArray(block.content) ? block.content.length : undefined,
        is_error: block.is_error ?? false,
      });
      continue;
    }
    const shape = { type: block.type };
    if (block.source?.type) shape.source_type = block.source.type;
    if (block.source?.media_type) shape.media_type = block.source.media_type;
    if (block.type === 'search_result') {
      shape.citations_enabled = block.citations?.enabled ?? false;
      shape.result_parts = Array.isArray(block.content) ? block.content.length : 0;
    }
    shapes.push(shape);
  }
  return shapes;
}

function requestContentShape(parsed) {
  return (parsed.messages ?? []).map((message) => ({
    role: message.role,
    content: contentShape(message.content),
  }));
}

function hasTool(parsed, name) {
  return (parsed.tools ?? []).some((t) => t.name === name);
}

// A reply is `{ text }` (end_turn), `{ tool: { id, name, input } }`, or one
// ordered `{ tools: [...] }` batch (tool_use). The batch form remains in this
// canonical fake-wire fixture so multi-call Runtime behavior is not modeled by
// a second test provider.
const text = (t) => ({ text: t });
const tool = (id, name, input) => ({ tool: { id, name, input } });
const toolBatch = (tools) => ({ tools });

// One canonical fake-wire implementation of the fixed Managed coordinator
// choreography. `send_to_agent` returns only an admission receipt; the child
// Agent's eventual response is projected independently on its Thread.
function managedCoordinationReply(parsed, agentId, message) {
  if (!hasTool(parsed, 'list_agents') || !hasTool(parsed, 'send_to_agent')) return null;
  // A completed child is delivered to the coordinator as a later internal User
  // message. The Anthropic wire shape does not retain Awaken's typed Message id,
  // so this fake-provider port recognizes the stable text envelope produced by
  // SessionApplication. It must finish that report Run without issuing another
  // send, or one child completion recursively creates another child.
  if (lastUserText(parsed).startsWith('Message from agent ')) {
    return text('coordination completed from child report');
  }
  const results = currentRunToolResults(parsed);
  const runOrdinal = userMessages(parsed).length;
  if (results.length === 0) return tool(`list-agents-${runOrdinal}`, 'list_agents', {});
  if (results.length === 1) {
    if (lastUserText(parsed) === 'follow up with the same child') {
      const sessionThreadId = latestCoordinatedThreadId(parsed);
      if (!sessionThreadId) throw new Error('follow-up has no prior coordinated Thread receipt');
      return tool(`send-agent-${runOrdinal}`, 'send_to_agent', {
        session_thread_id: sessionThreadId,
        message: 'confirm the previous research result from your retained history',
      });
    }
    return tool(`send-agent-${runOrdinal}`, 'send_to_agent', { agent_id: agentId, message });
  }
  const result = results.at(-1);
  return text(`coordination ${result.is_error ? 'failed' : 'accepted'}: ${toolResultText(result)}`);
}

function adminDriveReply(parsed) {
  switch (toolResults(parsed).length) {
    case 0: return tool('c0', 'admin_get_platform_capabilities', {});
    case 1: return tool('c1', 'admin_draft_agent', {
      id: 'drafted-agent',
      instructions: 'a drafted agent',
      resources: [
        { kind: 'file', resource_id: 'draft-file', access: 'read_write' },
        { kind: 'memory_store', resource_id: 'draft-memory', access: 'read_only' },
        { kind: 'repository', resource_id: 'draft-repository' },
      ],
    });
    case 2: return tool('c2', 'admin_patch_agent', {
      id: 'drafted-agent',
      patch: {
        description: 'patched by the admin assistant',
        resources: [{ kind: 'file', resource_id: 'replacement-file' }],
      },
    });
    case 3: return tool('c3', 'admin_validate_agent', { id: 'drafted-agent' });
    case 4: return tool('c4', 'admin_draft_environment', {
      name: 'admin-authored-environment',
      placement: 'self_hosted',
    });
    case 5: return tool('c5', 'admin_explain_console', { topic: 'agent' });
    default: return text('ADMIN-RUN-DONE: capabilities, agent draft, environment, and help completed');
  }
}

function adminTypedEdgesReply(parsed) {
  switch (toolResults(parsed).length) {
    case 0: return tool('typed-0', 'admin_draft_agent', {
      id: 'typed-admin-draft',
      instructions: 'exercise typed authoring normalization',
      mcp_servers: [{
        id: 'typed-mcp',
        url: 'https://mcp.example.invalid',
        credential: { id: 'cred_typed', revision: 3 },
      }],
      skills: ['skill-string', { id: 'skill-object' }],
      multiagent: { type: 'coordinator', agents: ['delegate-a'] },
    });
    case 1: return tool('typed-1', 'admin_draft_agent', {
      id: 'bad-mcp-shape',
      instructions: 'reject a non-object MCP entry',
      mcp_servers: ['not-an-object'],
    });
    case 2: return tool('typed-2', 'admin_draft_agent', {
      id: 'bad-mcp-credential',
      instructions: 'reject an invalid typed credential reference',
      mcp_servers: [{
        name: 'typed-mcp',
        url: 'https://mcp.example.invalid',
        credential: { id: 7, revision: 'not-a-revision' },
      }],
    });
    case 3: return tool('typed-3', 'admin_draft_agent', {
      id: 'bad-skill-shape',
      instructions: 'reject a non-id Skill entry',
      skills: [{ name: 'not-an-id' }],
    });
    case 4: return tool('typed-4', 'admin_draft_agent', {
      id: 'bad-roster-shape',
      instructions: 'reject an invalid delegation roster',
      multiagent: { type: 'mesh', agents: [] },
    });
    default: return text('ADMIN-TYPED-EDGES-DONE');
  }
}

export const BEHAVIORS = {
  // The historic default: echo the last user text, and drive one tool round-trip
  // on `use-tool:<name>` (kept so the fault-injection / real-wire e2e are unchanged).
  default(parsed) {
    if (hasTool(parsed, 'admin_get_platform_capabilities')) return adminDriveReply(parsed);
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
  // Outcome Worker + Judge behavior. Judge replies are strict JSON; Worker
  // revisions produce FINAL after the runtime-owned feedback Step.
  revise(parsed) {
    const users = allUserText(parsed);
    if (users.includes('Evaluate this Outcome input')) {
      const input = outcomeJudgeInput(parsed);
      if (input.rubric === 'INVALID_JUDGE_OUTPUT') return text('not a grade object');
      if (input.rubric === 'FORCE_FAILED_DECISION') {
        return text(JSON.stringify({
          result: 'failed',
          explanation: 'native judge rejected the deliverable terminally',
        }));
      }
      const satisfied = evaluatedOutcomeText(input).includes(input.rubric);
      return text(JSON.stringify({
        result: satisfied ? 'satisfied' : 'needs_revision',
        explanation: satisfied ? 'native judge accepted evidence' : 'native judge requests FINAL',
      }));
    }
    return text(users.includes('Revise the deliverable') ? 'FINAL answer' : 'a rough draft');
  },
  // VisionProbeModel: report the media types on the latest User message.
  vision(parsed) {
    const medias = lastUserImages(parsed);
    const t = blockText(((parsed.messages ?? []).filter((m) => m.role === 'user').pop() ?? {}).content);
    return text(medias.length ? `saw ${medias.join(',')}; text: ${t}` : `saw no media; text: ${t}`);
  },
  // CompactionModel: the compactor sub-Run returns a fixed summary; a main Run
  // prefixes its reply with the system/context text it received.
  compaction(parsed) {
    const sys = systemLines(parsed);
    if (allUserText(parsed).includes('summarize') || sys.includes('summar')) return text('SUMMARY: earlier Runs folded');
    return text(`ctx:[${sys}] echo:${lastUserText(parsed)}`);
  },
  // MemoryProbeModel: the extractor sub-run saves one memory via `write_memory`
  // (named after a `fact-<tag>` token when present); a main Run prefixes its echo
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
  // MemoryResourceModel cause/effect table (shared with the in-process scenario):
  // R1 no tool result -> write the marker; R2 exactly one result -> read the same
  // mounted path; R3 two results -> terminate. R2 makes the observation independent
  // of the host's later harvest API: a successful terminal Run must have observed
  // the exact bytes through the sandbox mount first.
  memoryResource(parsed) {
    switch (toolResults(parsed).length) {
      case 0: return tool('memres-1', 'write', { path: '/mnt/memory/note.md', content: lastUserText(parsed) });
      case 1: return tool('memres-2', 'read', { path: '/mnt/memory/note.md' });
      default: return text('memory persisted');
    }
  },
  // DreamScenarioModel over the real provider wire. A restart test deliberately
  // delays this response and kills the Coordinator after arrival, then verifies
  // that the durable Dream is re-dispatched and remains cancelable.
  dream(parsed) {
    if (!allUserText(parsed).includes('[dream-job:')) {
      return text(`Echo: ${lastUserText(parsed)}`);
    }
    if (toolResults(parsed).length > 0) return text('Dream consolidation written.');
    return tool('dream-write-through-1', 'write', {
      path: '/mnt/dream/output-memory/MEMORY.md',
      content: '# Dream\n- Consolidated by the Dream Agent.\n',
    });
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
      default: return text('repo Run done');
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
  // HandBrainLazyModel: separate Runs deliberately select one Brain tool and
  // one Sandbox tool. The e2e observes the durable Session environment binding
  // around each Run, proving placement rather than inferring it from output.
  handBrainLazy(parsed) {
    const results = toolResults(parsed);
    const msgs = parsed.messages ?? [];
    const last = msgs[msgs.length - 1];
    if (last && Array.isArray(last.content) && last.content.some((b) => b.type === 'tool_result')) {
      return text(`tool-result: ${toolResultText(results[results.length - 1])}`);
    }
    const prompt = lastUserText(parsed);
    if (prompt === 'brain') return tool(`brain-${msgs.length}`, 'mcp__calc__add', { a: 20, b: 22 });
    if (prompt === 'hand') return tool(`hand-${msgs.length}`, 'read', { path: 'missing-hand-e2e.txt' });
    return text(`Echo: ${prompt}`);
  },
  // FullChainModel (the native combined-chain e2e): one conversation that drives the
  // whole ADR-0038/0036 loop — read+use a filesystem Skill, write into the mounted memory
  // store, write into the cloned git repo, and produce an output artifact — plus its
  // out-of-band extractor sub-run that saves a memory. Sequenced by tool-result count
  // so it needs no transcript parsing; every write is host-gated and harvested.
  fullChain(parsed) {
    const prompt = lastUserText(parsed);
    if (prompt === 'probe-repository-skill-snapshot') {
      // This probe deliberately returns only prompt metadata and invokes no tool:
      // the E2E can distinguish startup discovery from successful file access.
      const paths = systemText(parsed)
        .split('`')
        .filter((part) => part.includes('/.claude/skills/') && part.endsWith('/SKILL.md'));
      return text(JSON.stringify(paths));
    }
    if (prompt === 'author-skill-v1' || prompt === 'author-skill-v2') {
      if (toolResults(parsed).length > 0) return text(`authored ${prompt}`);
      const body = prompt.endsWith('v2') ? 'AUTHORED_SKILL_V2' : 'AUTHORED_SKILL_V1';
      return tool(`author-${prompt}`, 'write', {
        path: 'skills/authored/SKILL.md',
        content: `---\nname: authored\ndescription: agent authored skill\n---\n${body}`,
      });
    }
    // The extraction sub-run (out-of-band): save one memory, then finish. It is seeded
    // with the whole main-Run transcript (which carries tool results), so we can't key
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
    const results = toolResults(parsed);
    const managedSkillPaths = systemText(parsed)
      .split('`')
      .filter((part) => part.endsWith('/SKILL.md'));
    const attachedSkillPath = managedSkillPaths.find((part) => part.includes('.skills/'));
    const repositorySkillPath = managedSkillPaths.find((part) => part.includes('/.claude/skills/'));
    switch (results.length) {
      case 0: {
        if (!attachedSkillPath) return text('missing attached filesystem Skill path');
        return tool('sk-read-attached', 'read', { path: attachedSkillPath });
      }
      case 1: {
        if (!repositorySkillPath) return text('missing repository filesystem Skill path');
        return tool('sk-read-repository', 'read', { path: repositorySkillPath });
      }
      // Write into the mounted MemoryStore directory.
      case 2: return tool('wm', 'write', { path: '/mnt/memory/note.md', content: 'MEMO_FULLCHAIN_5521' });
      // Write into the cloned repo working tree; lifecycle cleanup never pushes it.
      case 3: return tool('wr', 'write', { path: '/workspace/repo/CHAIN.txt', content: 'REPO_FULLCHAIN_8830' });
      // Produce an output artifact (harvested into the blob store, listed by /v1/files).
      case 4: return tool('wa', 'write', { path: '/mnt/session/outputs/result.txt', content: 'ARTIFACT_FULLCHAIN_9142' });
      // Commit and export the review handoff. The agent uses only its sandbox;
      // an external operator owns application, tests, scans, and remote push.
      case 5: return tool('wc', 'bash', { command: "out=${AWAKEN_OUTPUTS_DIR:?} && cd /workspace/repo && git add -A && git -c user.email=agent@awaken -c user.name=agent commit -m 'agent: add CHAIN.txt' && git format-patch -1 --stdout > \"$out/change.patch\" && base=$(git rev-parse HEAD^) && head=$(git rev-parse HEAD) && patch_sha=$(sha256sum \"$out/change.patch\" | awk '{print $1}') && printf '{\"base_commit\":\"%s\",\"sandbox_commit\":\"%s\",\"patch_sha256\":\"%s\"}\\n' \"$base\" \"$head\" \"$patch_sha\" > \"$out/manifest.json\" && sha256sum \"$out/manifest.json\" | awk '{print $1}' > \"$out/manifest.sha256\"" });
      default: return text('done: used attached and repository skills, wrote memory + repo + artifact');
    }
  },
  // SkillDrivingModel supports the one delivery selected by Session capability:
  // filesystem metadata -> `read` the advertised SKILL.md; filesystem-free ->
  // `list_skills` then `Skill`. Both converge on the same final instruction use.
  skills(parsed) {
    const msgs = parsed.messages ?? [];
    const last = msgs[msgs.length - 1];
    const results = last && Array.isArray(last.content) ? last.content.filter((b) => b.type === 'tool_result') : [];
    const managedSkillPath = systemText(parsed)
      .split('`')
      .find((part) => part.endsWith('/SKILL.md'));
    if (results.length === 0 && managedSkillPath) {
      return tool('r', 'read', { path: managedSkillPath });
    }
    if (results.length === 0) return tool('l', 'list_skills', {});
    const lastText = results.map(toolResultText).join('');
    if (lastText.includes('"skills"')) {
      return tool('s', 'Skill', { skill: catalogSkillId(results.at(-1)) });
    }
    return text(`USED-SKILL: ${lastText}`);
  },
  // AdminAssistantModel (ADR-0052): drive the seeded management assistant through all
  // six admin tools in one run, sequenced by tool-result count, then a
  // final marker. Every draft it passes is a valid ordinary config (auto-bound, no
  // tools) so the real DraftValidator accepts it.
  adminDrive(parsed) {
    return adminDriveReply(parsed);
  },
  adminTypedEdges(parsed) {
    return adminTypedEdgesReply(parsed);
  },
  // DelegatingModel: a Managed coordinator discovers the fixed roster and sends
  // asynchronously to Native `researcher`, ACP `acp-worker`, explicit self, or
  // `ghost`. A child answers on its own Thread; the parent only reports the
  // send receipt. The self-copy task answers directly to bound recursion.
  delegating(parsed) {
    const requested = lastUserText(parsed);
    if (requested.includes('self-copy task')) return text('self copy: 42');
    if (requested === 'Provide your advice for the primary agent now.') {
      return text('independent advisor advice: verify the durable evidence');
    }
    if (requested.includes('consult the advisor')) {
      const results = currentRunToolResults(parsed);
      if (results.length === 0) {
        return tool(`advisor-${userMessages(parsed).length}`, 'advisor', {});
      }
      const advice = toolResultText(results.at(-1));
      return text(
        advice.includes('Advisor consultation failed')
          ? 'root handled the generic advisor failure'
          : `root used advisor advice: ${advice}`,
      );
    }
    if (requested === 'requires-action child task') {
      const results = currentRunToolResults(parsed);
      if (results.length > 0) {
        return text(`requires-action child completed: ${results.map(toolResultText).join(' | ')}`);
      }
      return toolBatch([
        {
          id: 'interrupt-write',
          name: 'write',
          input: { path: 'managed-interrupt-one.txt', content: 'must not be written' },
        },
        {
          id: 'interrupt-bash',
          name: 'bash',
          input: { command: "printf '%s' 'must not execute'" },
        },
      ]);
    }
    if (requested === 'confirm the previous research result from your retained history') {
      const retained = (parsed.messages ?? []).some(
        (entry) => entry.role === 'assistant' && blockText(entry.content).includes('researched: 42'),
      );
      return text(retained ? 'follow-up retained researched: 42' : 'follow-up history missing');
    }
    const agentId = requested.includes('ghost')
      ? 'ghost'
      : requested.includes('acp agent')
        ? 'acp-worker'
      : requested.includes('self agent')
        ? 'assistant'
        : 'researcher';
    const message = agentId === 'assistant'
      ? 'self-copy task'
      : requested.includes('awaiting child')
        ? 'requires-action child task'
      : requested.includes('delegate lifecycle:')
        ? requested
        : 'do the research';
    return managedCoordinationReply(parsed, agentId, message) ?? text('researched: 42');
  },
};

// System messages arrive as the top-level `system` field; the compaction/memory
// models joined multiple System lines with " | ", but the wire concatenates them —
// so match on the concatenated text (a `contains` check, which is all they do).
function systemLines(parsed) {
  return systemText(parsed);
}

// `opts`: `{ behavior, models, failuresBeforeSuccess, alwaysFail, failArrivalKind,
// faultStatus, delayMs, firstDelayMs }`. `models` enables the same fixture's
// Anthropic model-directory endpoint for Provider Connection tests.
// `behavior` (default `'default'`) selects the reproduced scenario model; the
// fault-injection knobs drive the runtime's retry + circuit-breaker + error paths.
export function startFakeAnthropic(apiKey, opts = {}) {
  const {
    behavior = 'default',
    failuresBeforeSuccess = 0,
    alwaysFail = false,
    faultStatus = 503,
    delayMs = 0,
    firstDelayMs = 0,
    models = [],
    failModel = null,
    failArrivalKind = null,
  } = opts;
  const reply_of = BEHAVIORS[behavior];
  if (!reply_of) throw new Error(`unknown fake-anthropic behavior: ${behavior}`);
  const state = { requests: [], arrivals: [], unauthorized: 0, attempts: 0, received: 0 };
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', async () => {
      const presented = req.headers['x-api-key'] ?? (req.headers.authorization ?? '').replace(/^Bearer /, '');
      if (req.method === 'GET' && req.url.startsWith('/v1/models') && models.length > 0) {
        if (presented !== apiKey) {
          state.unauthorized += 1;
          res.writeHead(401, { 'content-type': 'application/json' });
          res.end(JSON.stringify({ type: 'error', error: { type: 'authentication_error', message: 'invalid x-api-key' } }));
          return;
        }
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify({
          data: models.map((id) => ({ id })),
          has_more: false,
          last_id: models.at(-1) ?? null,
        }));
        return;
      }
      // Count arrival before an optional response delay. Crash/recovery e2e use
      // this externally observable socket fact to kill a worker while inference
      // is genuinely in flight, without peeking into the Rust process.
      state.received += 1;
      const arrival = JSON.parse(body || '{}');
      // Outcome-arrival cause/effect rule A1: C1=a later command retains prior
      // Outcome prompts in its transcript; C2=the last User prompt names the
      // request currently arriving. Inspecting all history lets C1 overwrite
      // C2, while selecting C2 yields exactly one initial/revision/judge/ack
      // family and keeps failArrivalKind scoped to the current request.
      const arrivalUser = lastUserText(arrival);
      const arrivalKind = arrivalUser.includes('Evaluate this Outcome input') ? 'outcome-judge'
        : arrivalUser.includes('Revise the deliverable') ? 'outcome-revision'
          : arrivalUser.includes('iteration limit was reached') ? 'outcome-ack'
            : arrivalUser.includes('Work toward this Outcome') ? 'outcome-initial'
              : 'other';
      state.arrivals.push(arrivalKind);
      const responseDelay = state.received === 1 ? firstDelayMs || delayMs : delayMs;
      if (responseDelay > 0) await new Promise((r) => setTimeout(r, responseDelay));
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
      if (failArrivalKind === arrivalKind) {
        state.requests.push({ url: req.url, model: parsed.model, stream: !!parsed.stream, failed: true });
        res.writeHead(faultStatus, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'api_error', message: `${arrivalKind} failed` } }));
        return;
      }
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
      state.requests.push({
        url: req.url,
        model: parsed.model,
        stream: !!parsed.stream,
        // Test-only request evidence. Dynamic Session System
        // context is carried on the Anthropic top-level `system` field; keeping
        // its text in this existing request ledger lets lifecycle E2E prove the
        // context reached the provider instead of inferring it from model output.
        system: systemText(parsed),
        memoryExtractor: systemText(parsed).includes('memory extraction Agent'),
        contentShape: requestContentShape(parsed),
      });
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
          arrivals: state.arrivals,
          get unauthorized() {
            return state.unauthorized;
          },
          get attempts() {
            return state.attempts;
          },
          get received() {
            return state.received;
          },
          close: () => closeHttpServer(server),
        });
      });
    };
    listen();
  });
}

// The Anthropic non-streaming response: one text block or an ordered tool_use
// batch. Existing single-tool behaviors are normalized through the same path.
function emitJson(res, { id, model, reply }) {
  const toolCalls = reply.tools ?? (reply.tool ? [reply.tool] : []);
  const content = toolCalls.length > 0
    ? toolCalls.map((call) => ({ type: 'tool_use', id: call.id, name: call.name, input: call.input }))
    : [{ type: 'text', text: reply.text }];
  res.writeHead(200, { 'content-type': 'application/json' });
  res.end(
    JSON.stringify({
      id, type: 'message', role: 'assistant', model, content,
      stop_reason: toolCalls.length > 0 ? 'tool_use' : 'end_turn',
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

// The Anthropic streaming wire: the fixed event ladder around one text block or
// an ordered tool_use batch, so streaming and non-streaming infer assemble the
// same response.
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
  const toolCalls = reply.tools ?? (reply.tool ? [reply.tool] : []);
  if (toolCalls.length > 0) {
    for (const [index, call] of toolCalls.entries()) {
      ev('content_block_start', { type: 'content_block_start', index, content_block: { type: 'tool_use', id: call.id, name: call.name, input: {} } });
      // Chunk every call's arguments across several `input_json_delta` events
      // exactly as Anthropic streams `partial_json`. Each block keeps its own
      // index, so ordered multi-call batches are reconstructed without a fixture
      // side channel.
      for (const partial of chunkString(JSON.stringify(call.input), 4)) {
        ev('content_block_delta', { type: 'content_block_delta', index, delta: { type: 'input_json_delta', partial_json: partial } });
      }
      ev('content_block_stop', { type: 'content_block_stop', index });
    }
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
