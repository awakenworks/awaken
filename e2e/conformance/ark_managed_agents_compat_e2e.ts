// Ark -> Claude Managed Agents compatibility probe.
//
// This is deliberately an online TypeScript conformance test over the official
// @anthropic-ai/sdk. It does not duplicate the repository's Managed Agents
// client: one fetch adapter changes only the provider-specific route prefix
// (/v1 -> /api/v3), while the SDK continues to own DTOs, beta headers,
// pagination, SSE parsing, and error handling.
//
// Required:
//   ARK_API_TOKEN=... ARK_SESSION_ID=... npm run test:ark-managed-compat
//
// Full disposable-session lifecycle:
//   ARK_COMPAT_MUTATE=1 ARK_API_TOKEN=... ARK_SESSION_ID=... \
//     npm run test:ark-managed-compat
//
// A redacted interaction transcript is always written to
// e2e/artifacts/ark-managed-agents-compat.json (override with
// ARK_COMPAT_RECORDING). No authorization material or complete long text is
// retained.

import { createHash } from 'node:crypto';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
import type { Stream } from '@anthropic-ai/sdk/core/streaming';
import type { BetaManagedAgentsSession } from '@anthropic-ai/sdk/resources/beta/sessions/sessions';
import type {
  BetaManagedAgentsSessionEvent,
  BetaManagedAgentsStreamSessionEvents,
} from '@anthropic-ai/sdk/resources/beta/sessions/events';

type JsonPrimitive = string | number | boolean | null;
type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };
type JsonObject = { [key: string]: JsonValue };
type Verdict = 'pass' | 'fail' | 'skip';

interface Check {
  id: string;
  surface: string;
  verdict: Verdict;
  detail: string;
  evidence?: JsonValue;
}

interface Exchange {
  sequence: number;
  started_at: string;
  duration_ms: number;
  route_mode: 'direct' | 'ark_prefix_adapter';
  request: {
    method: string;
    sdk_path: string;
    effective_path: string;
    query: string;
    headers: Record<string, string>;
    body: JsonValue;
  };
  response: {
    status: number;
    content_type: string | null;
    request_id: string | null;
    body: JsonValue;
  };
}

interface Recording {
  schema_version: 1;
  generated_at: string;
  sdk: { package: '@anthropic-ai/sdk'; version: string };
  target: {
    api_origin: string;
    api_prefix: string;
    seed_session_id: string;
    mutation_enabled: boolean;
  };
  summary: {
    compatible: boolean;
    pass: number;
    fail: number;
    skip: number;
  };
  checks: Check[];
  exchanges: Exchange[];
}

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const E2E_DIR = path.resolve(SCRIPT_DIR, '..');
const DEFAULT_RECORDING = path.join(E2E_DIR, 'artifacts', 'ark-managed-agents-compat.json');
const MANAGED_BETA = 'managed-agents-2026-04-01';
const OFFICIAL_TOOLSET = 'agent_toolset_20260401';
function requiredEnvironment(name: 'ARK_API_TOKEN' | 'ARK_SESSION_ID'): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}

const token = requiredEnvironment('ARK_API_TOKEN');
const seedSessionId = requiredEnvironment('ARK_SESSION_ID');
const apiRoot = new URL(process.env.ARK_API_ROOT ?? 'https://ark.cn-beijing.volces.com/api/v3');
const mutationEnabled = process.env.ARK_COMPAT_MUTATE === '1';
const strict = process.env.ARK_COMPAT_STRICT !== '0';
const recordingPath = path.resolve(process.env.ARK_COMPAT_RECORDING ?? DEFAULT_RECORDING);
const pollTimeoutMs = Number(process.env.ARK_COMPAT_POLL_TIMEOUT_MS ?? 180_000);
const checks: Check[] = [];
const exchanges: Exchange[] = [];

const sdkPackage = JSON.parse(
  await readFile(path.join(E2E_DIR, 'node_modules', '@anthropic-ai', 'sdk', 'package.json'), 'utf8'),
) as { version: string };

const OFFICIAL_EVENT_TYPES = new Set([
  'user.message',
  'user.interrupt',
  'user.tool_confirmation',
  'user.custom_tool_result',
  'user.tool_result',
  'user.define_outcome',
  'system.message',
  'agent.message',
  'agent.thinking',
  'agent.tool_use',
  'agent.tool_result',
  'agent.mcp_tool_use',
  'agent.mcp_tool_result',
  'agent.custom_tool_use',
  'agent.thread_context_compacted',
  'agent.thread_message_received',
  'agent.thread_message_sent',
  'agent.session_thread_message_received',
  'agent.session_thread_message_sent',
  'session.error',
  'session.updated',
  'session.deleted',
  'session.status_running',
  'session.status_idle',
  'session.status_rescheduled',
  'session.status_terminated',
  'session.thread_created',
  'session.thread_status_created',
  'session.thread_status_running',
  'session.thread_status_idle',
  'session.thread_status_rescheduled',
  'session.thread_status_terminated',
  'span.model_request_start',
  'span.model_request_end',
  'span.outcome_evaluation_start',
  'span.outcome_evaluation_ongoing',
  'span.outcome_evaluation_end',
  'event_start',
  'event_delta',
]);

function sha256(value: string): string {
  return createHash('sha256').update(value).digest('hex');
}

function summarize(value: unknown, key = ''): JsonValue {
  if (value === null || value === undefined) return null;
  if (typeof value === 'number' || typeof value === 'boolean') return value;
  if (typeof value === 'string') {
    if (/^(authorization|authorization_token|api_?key|access_?token|secret|credential)$/i.test(key)) {
      return '[REDACTED]';
    }
    if (key === 'text' || value.length > 512) {
      return {
        preview: value.slice(0, 160),
        chars: value.length,
        sha256: sha256(value),
        truncated: value.length > 160,
      };
    }
    return value;
  }
  if (Array.isArray(value)) return value.map((item) => summarize(item));
  if (typeof value === 'object') {
    const result: JsonObject = {};
    for (const [childKey, childValue] of Object.entries(value)) {
      result[childKey] = summarize(childValue, childKey);
    }
    return result;
  }
  return String(value);
}

function safeHeaders(headers: Headers): Record<string, string> {
  const result: Record<string, string> = {};
  for (const [name, value] of headers.entries()) {
    const lower = name.toLowerCase();
    if (lower === 'authorization' || lower === 'x-api-key') {
      result[lower] = value.toLowerCase().startsWith('bearer ') ? 'Bearer [REDACTED]' : '[REDACTED]';
    } else if (lower === 'content-type' || lower === 'accept' || lower === 'anthropic-beta' ||
      lower === 'anthropic-version' || lower.startsWith('x-stainless')) {
      result[lower] = value;
    }
  }
  return result;
}

async function bodySummary(body: BodyInit | null | undefined): Promise<JsonValue> {
  if (body == null) return null;
  if (typeof body === 'string') {
    try {
      return summarize(JSON.parse(body));
    } catch {
      return summarize(body);
    }
  }
  return `[${body.constructor?.name ?? 'body'}]`;
}

async function captureSsePreview(response: Response, timeoutMs = 10_000): Promise<JsonValue> {
  const reader = response.body?.getReader();
  if (!reader) return { chars: 0, event_names: [], preview: '' };
  const decoder = new TextDecoder();
  let text = '';
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    void reader.cancel();
  }, timeoutMs);
  try {
    while (text.length < 32_768) {
      const { value, done } = await reader.read();
      if (done) break;
      text += decoder.decode(value, { stream: true });
    }
  } catch {
    // Cancelling the diagnostic clone is expected at the bounded timeout.
  } finally {
    clearTimeout(timer);
    await reader.cancel().catch(() => {});
  }
  const eventNames = [...text.matchAll(/^event:\s*([^\r\n]+)/gm)].map((match) => match[1]);
  const dataLines = [...text.matchAll(/^data:\s*(.*)$/gm)].map((match) => match[1]);
  return {
    chars: text.length,
    event_names: eventNames,
    data_line_count: dataLines.length,
    preview: text.slice(0, 1_000),
    timed_out: timedOut,
  };
}

function effectiveArkURL(original: URL): URL {
  if (!original.pathname.startsWith('/v1/')) return original;
  const effective = new URL(original);
  const prefix = apiRoot.pathname.replace(/\/$/, '');
  effective.pathname = `${prefix}${original.pathname.slice('/v1'.length)}`;
  return effective;
}

function recordingFetch(routeMode: Exchange['route_mode']): typeof fetch {
  return async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const original = new URL(typeof input === 'string' || input instanceof URL ? input : input.url);
    const effective = routeMode === 'ark_prefix_adapter' ? effectiveArkURL(original) : original;
    const headers = new Headers(init?.headers ?? (input instanceof Request ? input.headers : undefined));
    const started = Date.now();
    const startedAt = new Date(started).toISOString();
    let response: Response;
    let responseBody: JsonValue = null;
    try {
      response = await fetch(effective, init);
      const contentType = response.headers.get('content-type');
      if (contentType?.includes('application/json')) {
        const text = await response.clone().text();
        if (text) {
          try {
            responseBody = summarize(JSON.parse(text));
          } catch {
            responseBody = summarize(text);
          }
        }
      } else if (contentType?.includes('text/event-stream')) {
        responseBody = '[SSE stream retained by official SDK parser]';
      } else {
        const text = await response.clone().text();
        responseBody = text ? summarize(text) : null;
      }
      const exchange: Exchange = {
        sequence: exchanges.length + 1,
        started_at: startedAt,
        duration_ms: Date.now() - started,
        route_mode: routeMode,
        request: {
          method: init?.method ?? (input instanceof Request ? input.method : 'GET'),
          sdk_path: original.pathname,
          effective_path: effective.pathname,
          query: effective.search,
          headers: safeHeaders(headers),
          body: await bodySummary(init?.body),
        },
        response: {
          status: response.status,
          content_type: contentType,
          request_id: response.headers.get('x-request-id') ?? response.headers.get('request-id'),
          body: responseBody,
        },
      };
      exchanges.push(exchange);
      if (contentType?.includes('text/event-stream')) {
        void captureSsePreview(response.clone()).then((preview) => {
          exchange.response.body = preview;
        });
      }
      return response;
    } catch (error) {
      exchanges.push({
        sequence: exchanges.length + 1,
        started_at: startedAt,
        duration_ms: Date.now() - started,
        route_mode: routeMode,
        request: {
          method: init?.method ?? (input instanceof Request ? input.method : 'GET'),
          sdk_path: original.pathname,
          effective_path: effective.pathname,
          query: effective.search,
          headers: safeHeaders(headers),
          body: await bodySummary(init?.body),
        },
        response: {
          status: 0,
          content_type: null,
          request_id: null,
          body: summarize(error instanceof Error ? error.message : String(error)),
        },
      });
      throw error;
    }
  };
}

function clientFor(routeMode: Exchange['route_mode']): Anthropic {
  const baseURL = routeMode === 'direct' ? apiRoot.toString().replace(/\/$/, '') : apiRoot.origin;
  return new Anthropic({
    authToken: token,
    baseURL,
    fetch: recordingFetch(routeMode),
    maxRetries: 0,
    timeout: pollTimeoutMs,
  });
}

function addCheck(
  id: string,
  surface: string,
  verdict: Verdict,
  detail: string,
  evidence?: unknown,
): void {
  checks.push({ id, surface, verdict, detail, ...(evidence === undefined ? {} : { evidence: summarize(evidence) }) });
  const marker = verdict === 'pass' ? 'PASS' : verdict === 'fail' ? 'FAIL' : 'SKIP';
  console.log(`${marker} ${id}: ${detail}`);
}

function errorEvidence(error: unknown): JsonObject {
  const candidate = error as {
    name?: string;
    status?: number;
    message?: string;
    error?: unknown;
  };
  return {
    name: candidate?.name ?? 'Error',
    status: candidate?.status ?? null,
    message: candidate?.message ?? String(error),
    error: summarize(candidate?.error),
  };
}

function asObject(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function missingKeys(value: unknown, keys: string[]): string[] {
  const object = asObject(value);
  return keys.filter((key) => !Object.prototype.hasOwnProperty.call(object, key));
}

function validateSession(value: unknown): string[] {
  const issues: string[] = [];
  const session = asObject(value);
  const missing = missingKeys(session, [
    'id',
    'agent',
    'archived_at',
    'created_at',
    'environment_id',
    'metadata',
    'outcome_evaluations',
    'resources',
    'stats',
    'status',
    'title',
    'type',
    'updated_at',
    'usage',
    'vault_ids',
  ]);
  if (missing.length) issues.push(`session missing required keys: ${missing.join(', ')}`);
  if (session.type !== 'session') issues.push(`session.type=${String(session.type)}`);
  const agent = asObject(session.agent);
  const missingAgent = missingKeys(agent, [
    'id',
    'description',
    'mcp_servers',
    'model',
    'multiagent',
    'name',
    'skills',
    'system',
    'tools',
    'type',
    'version',
  ]);
  if (missingAgent.length) issues.push(`session.agent missing required keys: ${missingAgent.join(', ')}`);
  const tools = Array.isArray(agent.tools) ? agent.tools : [];
  for (const tool of tools) {
    const kind = asObject(tool).type;
    if (typeof kind !== 'string') {
      issues.push('session.agent.tools contains an item without type');
    } else if (![OFFICIAL_TOOLSET, 'mcp_toolset', 'custom'].includes(kind)) {
      issues.push(`unsupported official SDK tool discriminator: ${kind}`);
    }
  }
  return issues;
}

function validatePage(page: unknown): string[] {
  const body = asObject(asObject(page).body);
  const issues: string[] = [];
  if (!Array.isArray(body.data)) issues.push('page body is missing data[]');
  if (!Object.prototype.hasOwnProperty.call(body, 'next_page')) issues.push('page body is missing next_page');
  return issues;
}

function validateEvent(value: unknown, persisted = true): string[] {
  const event = asObject(value);
  const issues: string[] = [];
  if (typeof event.type !== 'string') return ['event.type is missing'];
  if (!OFFICIAL_EVENT_TYPES.has(event.type)) issues.push(`unknown official SDK event type: ${event.type}`);
  if (!['event_start', 'event_delta'].includes(event.type)) {
    if (typeof event.id !== 'string' || !event.id) issues.push(`${event.type} missing id`);
    if (persisted && (typeof event.processed_at !== 'string' || !event.processed_at)) {
      issues.push(`${event.type} missing committed processed_at`);
    }
  }
  if ((event.type === 'agent.message' || event.type === 'user.message') && !Array.isArray(event.content)) {
    issues.push(`${event.type} missing content[]`);
  }
  if (event.type === 'agent.thinking' && Object.prototype.hasOwnProperty.call(event, 'content')) {
    issues.push('agent.thinking must be a contentless progress marker');
  }
  if (event.type === 'agent.tool_use') {
    if (typeof event.name !== 'string') issues.push('agent.tool_use missing name');
    if (!Object.prototype.hasOwnProperty.call(event, 'input')) issues.push('agent.tool_use missing input');
  }
  if (event.type === 'agent.tool_result' && typeof event.tool_use_id !== 'string') {
    issues.push('agent.tool_result missing tool_use_id');
  }
  if (event.type === 'session.status_idle') {
    const stopReason = asObject(event.stop_reason);
    if (!['end_turn', 'requires_action', 'retries_exhausted'].includes(String(stopReason.type))) {
      issues.push(`session.status_idle invalid stop_reason: ${String(stopReason.type)}`);
    }
  }
  if (event.type === 'span.model_request_end') {
    for (const key of ['is_error', 'model_request_start_id', 'model_usage']) {
      if (!Object.prototype.hasOwnProperty.call(event, key)) {
        issues.push(`span.model_request_end missing ${key}`);
      }
    }
  }
  return issues;
}

async function waitForSessionIdle(
  client: Anthropic,
  sessionId: string,
  timeoutMs = pollTimeoutMs,
): Promise<BetaManagedAgentsSession> {
  const deadline = Date.now() + timeoutMs;
  let last: BetaManagedAgentsSession | undefined;
  while (Date.now() < deadline) {
    last = await client.beta.sessions.retrieve(sessionId, { betas: [MANAGED_BETA] });
    if (last.status === 'idle' || last.status === 'terminated') return last;
    await new Promise((resolve) => setTimeout(resolve, 1_000));
  }
  throw new Error(`timed out waiting for ${sessionId} to become idle; last status=${last?.status}`);
}

async function listEvents(
  client: Anthropic,
  sessionId: string,
): Promise<{ page: unknown; events: BetaManagedAgentsSessionEvent[] }> {
  const first = await client.beta.sessions.events.list(sessionId, {
    limit: 100,
    betas: [MANAGED_BETA],
  });
  const events: BetaManagedAgentsSessionEvent[] = [];
  for await (const event of first) events.push(event);
  return { page: first, events };
}

async function collectStream(
  stream: Stream<BetaManagedAgentsStreamSessionEvents>,
  stop: (event: BetaManagedAgentsStreamSessionEvents, seen: BetaManagedAgentsStreamSessionEvents[]) => boolean,
  timeoutMs = pollTimeoutMs,
): Promise<{ events: BetaManagedAgentsStreamSessionEvents[]; timed_out: boolean }> {
  const seen: BetaManagedAgentsStreamSessionEvents[] = [];
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    stream.controller.abort();
  }, timeoutMs);
  try {
    for await (const event of stream) {
      seen.push(event);
      if (stop(event, seen)) break;
    }
  } finally {
    clearTimeout(timer);
    stream.controller.abort();
  }
  return { events: seen, timed_out: timedOut };
}

async function main(): Promise<void> {
  const directClient = clientFor('direct');
  try {
    await directClient.beta.sessions.retrieve(seedSessionId, { betas: [MANAGED_BETA] });
    addCheck('transport.drop_in_route', 'transport', 'pass', 'official SDK route worked without adaptation');
  } catch (error) {
    addCheck(
      'transport.drop_in_route',
      'transport',
      'fail',
      'official SDK appends /v1, so Ark /api/v3 is not a drop-in baseURL',
      errorEvidence(error),
    );
  }

  const client = clientFor('ark_prefix_adapter');
  let seed: BetaManagedAgentsSession;
  try {
    seed = await client.beta.sessions.retrieve(seedSessionId, { betas: [MANAGED_BETA] });
    addCheck('sessions.retrieve', 'sessions', 'pass', 'seed session is retrievable through the route-prefix adapter', {
      id: seed.id,
      status: seed.status,
    });
  } catch (error) {
    addCheck('sessions.retrieve', 'sessions', 'fail', 'adapted session retrieval failed', errorEvidence(error));
    return;
  }

  const seedShapeIssues = validateSession(seed);
  addCheck(
    'sessions.retrieve.schema',
    'sessions',
    seedShapeIssues.length ? 'fail' : 'pass',
    seedShapeIssues.length ? seedShapeIssues.join('; ') : 'session matches the official required response shape',
  );

  try {
    const sessionsPage = await client.beta.sessions.list({ limit: 2, betas: [MANAGED_BETA] });
    const issues = validatePage(sessionsPage);
    addCheck('sessions.list', 'sessions', 'pass', 'official SDK listed sessions', {
      count: sessionsPage.data.length,
      next_page: sessionsPage.next_page,
    });
    addCheck(
      'sessions.list.schema',
      'sessions',
      issues.length ? 'fail' : 'pass',
      issues.length ? issues.join('; ') : 'session list and cursor envelope match the SDK contract',
    );
  } catch (error) {
    addCheck('sessions.list', 'sessions', 'fail', 'session listing failed', errorEvidence(error));
  }

  try {
    const history = await listEvents(client, seedSessionId);
    const eventIssues = history.events.flatMap((event, index) =>
      validateEvent(event).map((issue) => `event[${index}]: ${issue}`)
    );
    addCheck(
      'events.seed_history',
      'events',
      eventIssues.length ? 'fail' : 'pass',
      eventIssues.length ? eventIssues.join('; ') : 'seed event history matches the official event catalog and shapes',
      {
        count: history.events.length,
        types: [...new Set(history.events.map((event) => event.type))].sort(),
      },
    );
    const pageIssues = validatePage(history.page);
    addCheck(
      'events.seed_history.page_schema',
      'events',
      pageIssues.length ? 'fail' : 'pass',
      pageIssues.length ? pageIssues.join('; ') : 'seed event page matches the SDK cursor envelope',
    );
  } catch (error) {
    addCheck('events.seed_history', 'events', 'fail', 'seed event history failed', errorEvidence(error));
  }

  if (!mutationEnabled) {
    addCheck(
      'lifecycle.disposable_session',
      'lifecycle',
      'skip',
      'set ARK_COMPAT_MUTATE=1 to run create/update/events/stream/threads/archive/delete',
    );
    return;
  }

  const seedObject = seed as unknown as Record<string, unknown>;
  const agent = asObject(seedObject.agent);
  const agentId = process.env.ARK_COMPAT_AGENT_ID ?? String(agent.id ?? '');
  const environmentId = process.env.ARK_COMPAT_ENVIRONMENT_ID ?? String(seedObject.environment_id ?? '');
  if (!agentId || !environmentId) {
    addCheck('sessions.create', 'sessions', 'fail', 'could not infer disposable session agent/environment IDs');
    return;
  }

  let disposableId: string | undefined;
  let primaryThreadId: string | undefined;
  try {
    let created: BetaManagedAgentsSession;
    try {
      created = await client.beta.sessions.create({
        agent: agentId,
        environment_id: environmentId,
        title: `Ark Managed compatibility ${new Date().toISOString()}`,
        metadata: { compatibility_probe: 'official-anthropic-typescript-sdk' },
        betas: [MANAGED_BETA],
      });
      disposableId = created.id;
      const issues = validateSession(created);
      addCheck('sessions.create', 'sessions', 'pass', 'official SDK created a disposable session', {
        id: created.id,
        status: created.status,
      });
      addCheck(
        'sessions.create.schema',
        'sessions',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'disposable session created with official request and response shapes',
      );
    } catch (error) {
      addCheck('sessions.create', 'sessions', 'fail', 'disposable session creation failed', errorEvidence(error));
      return;
    }

    try {
      const updated = await client.beta.sessions.update(disposableId, {
        title: 'Ark Managed compatibility updated',
        metadata: { compatibility_probe: 'updated' },
        betas: [MANAGED_BETA],
      });
      const issues = validateSession(updated);
      const updatedObject = updated as unknown as Record<string, unknown>;
      const updatedMetadata = asObject(updatedObject.metadata);
      const updateIssues = [];
      if (updated.title !== 'Ark Managed compatibility updated') updateIssues.push('title did not round-trip');
      if (updatedMetadata.compatibility_probe !== 'updated') updateIssues.push('metadata did not round-trip');
      addCheck(
        'sessions.update',
        'sessions',
        updateIssues.length ? 'fail' : 'pass',
        updateIssues.length ? updateIssues.join('; ') : 'title/metadata update round-tripped',
        { title: updated.title, metadata: updatedObject.metadata ?? null },
      );
      addCheck(
        'sessions.update.schema',
        'sessions',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'updated session matches the official response shape',
      );
    } catch (error) {
      addCheck('sessions.update', 'sessions', 'fail', 'session update failed', errorEvidence(error));
    }

    try {
      const receipt = await client.beta.sessions.events.send(disposableId, {
        events: [{
          type: 'system.message',
          content: [{ type: 'text', text: 'Compatibility probe: keep responses concise.' }],
        }],
        betas: [MANAGED_BETA],
      });
      addCheck('events.system_message', 'events', 'pass', 'system.message was accepted', receipt);
    } catch (error) {
      addCheck('events.system_message', 'events', 'fail', 'system.message failed', errorEvidence(error));
    }

    let streamed: BetaManagedAgentsStreamSessionEvents[] = [];
    try {
      const stream = await client.beta.sessions.events.stream(disposableId, {
        event_deltas: ['agent.message', 'agent.thinking'],
        betas: [MANAGED_BETA],
      });
      const collecting = collectStream(
        stream,
        (event, seen) =>
          event.type === 'session.status_idle' &&
          seen.some((candidate) => candidate.type === 'session.status_running'),
      );
      const receipt = await client.beta.sessions.events.send(disposableId, {
        events: [{
          type: 'user.message',
          content: [{
            type: 'text',
            text: '必须使用 bash 执行 printf ARK_MANAGED_COMPAT_OK，然后只回复命令输出。',
          }],
        }],
        betas: [MANAGED_BETA],
      });
      const collected = await collecting;
      streamed = collected.events;
      const issues = streamed.flatMap((event, index) =>
        validateEvent(event, !['event_start', 'event_delta'].includes(event.type))
          .map((issue) => `stream[${index}]: ${issue}`)
      );
      const types = streamed.map((event) => event.type);
      for (const required of ['session.status_running', 'agent.message', 'session.status_idle']) {
        if (!types.includes(required as BetaManagedAgentsStreamSessionEvents['type'])) {
          issues.push(`stream missing ${required}`);
        }
      }
      if (collected.timed_out) issues.push('SSE stream timed out before terminal idle');
      addCheck(
        'events.stream_and_send',
        'events',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'SSE + user.message + deltas completed through official SDK',
        { receipt, types },
      );
    } catch (error) {
      addCheck('events.stream_and_send', 'events', 'fail', 'SSE/send interaction failed', errorEvidence(error));
    }

    try {
      await waitForSessionIdle(client, disposableId);
      const history = await listEvents(client, disposableId);
      const types = history.events.map((event) => event.type);
      const issues = history.events.flatMap((event, index) =>
        validateEvent(event).map((issue) => `event[${index}]: ${issue}`)
      );
      for (const required of [
        'user.message',
        'session.status_running',
        'agent.tool_use',
        'agent.tool_result',
        'agent.message',
        'session.status_idle',
      ]) {
        if (!types.includes(required as BetaManagedAgentsSessionEvent['type'])) {
          issues.push(`history missing ${required}`);
        }
      }
      addCheck(
        'events.tool_roundtrip',
        'events',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'built-in tool use/result and final answer were persisted',
        { types },
      );

      const first = await client.beta.sessions.events.list(disposableId, {
        limit: 2,
        betas: [MANAGED_BETA],
      });
      const paginationIssues = validatePage(first);
      if (first.data.length > 2) paginationIssues.push(`limit=2 returned ${first.data.length} events`);
      if (first.data.length === 2 && !first.next_page) paginationIssues.push('full first page has no next_page');
      if (first.hasNextPage()) {
        const second = await first.getNextPage();
        if (second.data.length === 0) paginationIssues.push('next_page resolved to an empty second page');
      }
      addCheck(
        'events.pagination',
        'events',
        paginationIssues.length ? 'fail' : 'pass',
        paginationIssues.length ? paginationIssues.join('; ') : 'event cursor pagination works',
      );
    } catch (error) {
      addCheck('events.tool_roundtrip', 'events', 'fail', 'event history/tool validation failed', errorEvidence(error));
    }

    let customToolConfigured = false;
    try {
      await client.beta.sessions.update(disposableId, {
        agent: {
          tools: [{
            type: 'custom',
            name: 'compat_echo',
            description: 'Required compatibility probe tool. Echo the provided value.',
            input_schema: {
              type: 'object',
              properties: { value: { type: 'string' } },
              required: ['value'],
            },
          }],
          mcp_servers: [],
        },
        betas: [MANAGED_BETA],
      });
      customToolConfigured = true;
      addCheck('sessions.update.custom_tool', 'sessions', 'pass', 'official custom-tool update was accepted');
    } catch (error) {
      addCheck(
        'sessions.update.custom_tool',
        'sessions',
        'fail',
        'official custom-tool update failed',
        errorEvidence(error),
      );
    }

    if (customToolConfigured) {
      try {
        await client.beta.sessions.events.send(disposableId, {
          events: [{
            type: 'user.message',
            content: [{
              type: 'text',
              text: '必须调用 compat_echo，参数 value 为 CUSTOM_TOOL_OK，然后回复工具结果。',
            }],
          }],
          betas: [MANAGED_BETA],
        });
        await waitForSessionIdle(client, disposableId);
        let history = await listEvents(client, disposableId);
        const customUse = [...history.events].reverse().find((event) => event.type === 'agent.custom_tool_use');
        const customUseId = String(asObject(customUse).id ?? '');
        if (!customUseId) throw new Error('agent.custom_tool_use was not emitted');
        await client.beta.sessions.events.send(disposableId, {
          events: [{
            type: 'user.custom_tool_result',
            custom_tool_use_id: customUseId,
            content: [{ type: 'text', text: 'CUSTOM_TOOL_OK' }],
          }],
          betas: [MANAGED_BETA],
        });
        await waitForSessionIdle(client, disposableId);
        history = await listEvents(client, disposableId);
        const resultReceipt = history.events.some((event) =>
          event.type === 'user.custom_tool_result' &&
          String(asObject(event).custom_tool_use_id ?? '') === customUseId
        );
        const finalMessage = history.events.some((event) => event.type === 'agent.message');
        const issues = [];
        if (!resultReceipt) issues.push('history missing user.custom_tool_result');
        if (!finalMessage) issues.push('history missing agent.message after custom result');
        addCheck(
          'events.custom_tool_result',
          'events',
          issues.length ? 'fail' : 'pass',
          issues.length ? issues.join('; ') : 'custom tool use/result resumed to a final agent message',
          { custom_tool_use_id: customUseId },
        );
      } catch (error) {
        addCheck(
          'events.custom_tool_result',
          'events',
          'fail',
          'custom tool interaction failed',
          errorEvidence(error),
        );
      }
    }

    let confirmationConfigured = false;
    try {
      await waitForSessionIdle(client, disposableId);
      await client.beta.sessions.update(disposableId, {
        agent: {
          tools: [{
            type: 'agent_toolset_20260401',
            default_config: {
              enabled: false,
              permission_policy: { type: 'always_ask' },
            },
            configs: [{
              name: 'bash',
              enabled: true,
              permission_policy: { type: 'always_ask' },
            }],
          }],
          mcp_servers: [],
        },
        betas: [MANAGED_BETA],
      });
      confirmationConfigured = true;
      addCheck(
        'sessions.update.always_ask',
        'sessions',
        'pass',
        'official agent_toolset_20260401 always_ask update was accepted',
      );
    } catch (error) {
      addCheck(
        'sessions.update.always_ask',
        'sessions',
        'fail',
        'official always_ask toolset update failed',
        errorEvidence(error),
      );
    }

    if (confirmationConfigured) {
      try {
        await client.beta.sessions.events.send(disposableId, {
          events: [{
            type: 'user.message',
            content: [{
              type: 'text',
              text: '必须使用 bash 执行 printf TOOL_CONFIRMATION_OK，然后回复输出。',
            }],
          }],
          betas: [MANAGED_BETA],
        });
        await waitForSessionIdle(client, disposableId);
        let history = await listEvents(client, disposableId);
        const toolUse = [...history.events].reverse().find((event) => event.type === 'agent.tool_use');
        const toolUseId = String(asObject(toolUse).id ?? '');
        const pending = [...history.events].reverse().find((event) => {
          if (event.type !== 'session.status_idle') return false;
          const stopReason = asObject(asObject(event).stop_reason);
          return stopReason.type === 'requires_action' &&
            Array.isArray(stopReason.event_ids) &&
            stopReason.event_ids.includes(toolUseId);
        });
        if (!toolUseId || !pending) throw new Error('always_ask did not emit a requires_action tool use');
        await client.beta.sessions.events.send(disposableId, {
          events: [{ type: 'user.tool_confirmation', tool_use_id: toolUseId, result: 'allow' }],
          betas: [MANAGED_BETA],
        });
        await waitForSessionIdle(client, disposableId);
        history = await listEvents(client, disposableId);
        const result = history.events.find((event) =>
          event.type === 'agent.tool_result' && String(asObject(event).tool_use_id ?? '') === toolUseId
        );
        addCheck(
          'events.tool_confirmation',
          'events',
          result ? 'pass' : 'fail',
          result
            ? 'requires_action + user.tool_confirmation resumed and executed the tool'
            : 'confirmation was accepted but matching agent.tool_result is absent',
          { tool_use_id: toolUseId },
        );
      } catch (error) {
        addCheck(
          'events.tool_confirmation',
          'events',
          'fail',
          'tool confirmation interaction failed',
          errorEvidence(error),
        );
      }
    }

    try {
      await waitForSessionIdle(client, disposableId);
      await client.beta.sessions.events.send(disposableId, {
        events: [{
          type: 'user.define_outcome',
          description: 'Reply with exactly OUTCOME_OK.',
          rubric: { type: 'text', content: 'The final answer must contain OUTCOME_OK.' },
          max_iterations: 1,
        }],
        betas: [MANAGED_BETA],
      });
      await waitForSessionIdle(client, disposableId);
      const history = await listEvents(client, disposableId);
      const types = history.events.map((event) => event.type);
      const issues = [];
      for (const required of [
        'user.define_outcome',
        'span.outcome_evaluation_start',
        'span.outcome_evaluation_end',
      ]) {
        if (!types.includes(required as BetaManagedAgentsSessionEvent['type'])) {
          issues.push(`history missing ${required}`);
        }
      }
      addCheck(
        'events.define_outcome',
        'events',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'define-outcome evaluation lifecycle completed',
      );
    } catch (error) {
      addCheck('events.define_outcome', 'events', 'fail', 'define-outcome interaction failed', errorEvidence(error));
    }

    try {
      const threadPage = await client.beta.sessions.threads.list(disposableId, {
        limit: 20,
        betas: [MANAGED_BETA],
      });
      const issues = validatePage(threadPage);
      primaryThreadId = threadPage.data[0]?.id;
      addCheck(
        'threads.list',
        'threads',
        primaryThreadId ? 'pass' : 'fail',
        primaryThreadId ? 'official SDK listed the primary session thread' : 'session has no primary thread',
        { ids: threadPage.data.map((thread) => thread.id) },
      );
      addCheck(
        'threads.list.schema',
        'threads',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'thread list matches cursor envelope',
      );

      if (primaryThreadId) {
        const thread = await client.beta.sessions.threads.retrieve(primaryThreadId, {
          session_id: disposableId,
          betas: [MANAGED_BETA],
        });
        const missing = missingKeys(thread, [
          'id',
          'agent',
          'archived_at',
          'created_at',
          'parent_thread_id',
          'session_id',
          'stats',
          'status',
          'type',
          'updated_at',
          'usage',
        ]);
        addCheck(
          'threads.retrieve',
          'threads',
          missing.length ? 'fail' : 'pass',
          missing.length ? `thread missing required keys: ${missing.join(', ')}` : 'primary thread shape matches SDK',
        );

        const threadEvents = await client.beta.sessions.threads.events.list(primaryThreadId, {
          session_id: disposableId,
          limit: 100,
          betas: [MANAGED_BETA],
        });
        const threadEventIssues = [
          ...threadEvents.data.flatMap((event, index) =>
            validateEvent(event).map((issue) => `event[${index}]: ${issue}`)
          ),
        ];
        addCheck(
          'threads.events.list',
          'threads',
          threadEventIssues.length ? 'fail' : 'pass',
          threadEventIssues.length ? threadEventIssues.join('; ') : 'thread event history matches SDK',
          { types: threadEvents.data.map((event) => event.type) },
        );
        const threadPageIssues = validatePage(threadEvents);
        addCheck(
          'threads.events.list.page_schema',
          'threads',
          threadPageIssues.length ? 'fail' : 'pass',
          threadPageIssues.length ? threadPageIssues.join('; ') : 'thread event page matches SDK cursor envelope',
        );

        try {
          const threadStream = await client.beta.sessions.threads.events.stream(primaryThreadId, {
            session_id: disposableId,
            betas: [MANAGED_BETA],
          });
          const collecting = collectStream(
            threadStream,
            (event, seen) =>
              event.type === 'session.status_idle' &&
              seen.some((candidate) => candidate.type === 'session.status_running'),
            60_000,
          );
          await client.beta.sessions.events.send(disposableId, {
            events: [{
              type: 'user.message',
              content: [{ type: 'text', text: '只回复 THREAD_STREAM_OK，不要使用工具。' }],
            }],
            betas: [MANAGED_BETA],
          });
          const observed = await collecting;
          addCheck(
            'threads.events.stream',
            'threads',
            observed.events.some((event) => event.type === 'agent.message') ? 'pass' : 'fail',
            observed.events.some((event) => event.type === 'agent.message')
              ? 'thread SSE delivered the live turn through the official SDK'
              : 'thread SSE produced no SDK-parsed agent.message during a live turn',
            { types: observed.events.map((event) => event.type), timed_out: observed.timed_out },
          );
        } catch (error) {
          addCheck('threads.events.stream', 'threads', 'fail', 'thread SSE failed', errorEvidence(error));
        }
      }
    } catch (error) {
      addCheck('threads.list', 'threads', 'fail', 'thread interactions failed', errorEvidence(error));
    }

    try {
      const resources = await client.beta.sessions.resources.list(disposableId, {
        limit: 20,
        betas: [MANAGED_BETA],
      });
      const issues = validatePage(resources);
      addCheck('resources.list', 'resources', 'pass', 'official SDK listed session resources', {
        count: resources.data.length,
      });
      addCheck(
        'resources.list.schema',
        'resources',
        issues.length ? 'fail' : 'pass',
        issues.length ? issues.join('; ') : 'resource list matches SDK cursor envelope',
      );
      if (resources.data.length === 0) {
        addCheck(
          'resources.crud',
          'resources',
          'skip',
          'no disposable Files API file_id was supplied; add/retrieve/update/delete cannot be safely exercised',
        );
      }
    } catch (error) {
      addCheck('resources.list', 'resources', 'fail', 'resource listing failed', errorEvidence(error));
    }

    try {
      const interrupted = await client.beta.sessions.events.send(disposableId, {
        events: [{ type: 'user.interrupt' }],
        betas: [MANAGED_BETA],
      });
      addCheck('events.interrupt', 'events', 'pass', 'user.interrupt was accepted', interrupted);
    } catch (error) {
      addCheck('events.interrupt', 'events', 'fail', 'user.interrupt failed', errorEvidence(error));
    }

    addCheck(
      'events.user_tool_result',
      'events',
      'skip',
      'user.tool_result is defined only for self_hosted environments; the supplied environment is cloud',
    );
    addCheck(
      'events.mcp_tool_result',
      'events',
      'skip',
      'the supplied agent has no MCP server, so no legitimate agent.mcp_tool_use can be resolved',
    );

    if (primaryThreadId) {
      try {
        await waitForSessionIdle(client, disposableId);
        const archivedThread = await client.beta.sessions.threads.archive(primaryThreadId, {
          session_id: disposableId,
          betas: [MANAGED_BETA],
        });
        addCheck('threads.archive', 'threads', 'pass', 'primary disposable thread archived', {
          id: archivedThread.id,
          status: archivedThread.status,
        });
      } catch (error) {
        addCheck('threads.archive', 'threads', 'fail', 'thread archive failed', errorEvidence(error));
      }
    }

    try {
      const current = await client.beta.sessions.retrieve(disposableId, { betas: [MANAGED_BETA] });
      if (current.status === 'running' || current.status === 'rescheduling') {
        await client.beta.sessions.events.send(disposableId, {
          events: [{ type: 'user.interrupt' }],
          betas: [MANAGED_BETA],
        });
        await waitForSessionIdle(client, disposableId);
      }
      const archived = await client.beta.sessions.archive(disposableId, { betas: [MANAGED_BETA] });
      addCheck('sessions.archive', 'lifecycle', 'pass', 'disposable session archived', {
        id: archived.id,
        status: archived.status,
        archived_at: archived.archived_at,
      });
    } catch (error) {
      addCheck('sessions.archive', 'lifecycle', 'fail', 'session archive failed', errorEvidence(error));
    }
  } finally {
    if (disposableId) {
      try {
        const deleted = await client.beta.sessions.delete(disposableId, { betas: [MANAGED_BETA] });
        addCheck('sessions.delete', 'lifecycle', 'pass', 'disposable session deleted', deleted);
      } catch (error) {
        addCheck('sessions.delete', 'lifecycle', 'fail', 'disposable session cleanup failed', errorEvidence(error));
      }
    }
  }
}

async function writeRecording(): Promise<Recording> {
  const summary = {
    compatible: checks.every((check) => check.verdict !== 'fail'),
    pass: checks.filter((check) => check.verdict === 'pass').length,
    fail: checks.filter((check) => check.verdict === 'fail').length,
    skip: checks.filter((check) => check.verdict === 'skip').length,
  };
  const recording: Recording = {
    schema_version: 1,
    generated_at: new Date().toISOString(),
    sdk: { package: '@anthropic-ai/sdk', version: sdkPackage.version },
    target: {
      api_origin: apiRoot.origin,
      api_prefix: apiRoot.pathname.replace(/\/$/, ''),
      seed_session_id: seedSessionId,
      mutation_enabled: mutationEnabled,
    },
    summary,
    checks,
    exchanges,
  };
  await mkdir(path.dirname(recordingPath), { recursive: true });
  await writeFile(recordingPath, `${JSON.stringify(recording, null, 2)}\n`, 'utf8');
  return recording;
}

let fatal: unknown;
try {
  await main();
} catch (error) {
  fatal = error;
  addCheck('probe.unhandled', 'probe', 'fail', 'unhandled compatibility probe failure', errorEvidence(error));
} finally {
  const recording = await writeRecording();
  console.log(`Recording: ${recordingPath}`);
  console.log(
    `SUMMARY compatible=${recording.summary.compatible} pass=${recording.summary.pass} ` +
      `fail=${recording.summary.fail} skip=${recording.summary.skip}`,
  );
  if (fatal || (strict && !recording.summary.compatible)) process.exitCode = 1;
}
