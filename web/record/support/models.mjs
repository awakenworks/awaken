import http from "node:http";
import os from "node:os";
import { setTimeout as delay } from "node:timers/promises";
import { BACKEND } from "./control-plane.mjs";

const SYNTHETIC_KEY = "recording-fixture-only"; // awaken-allow: secret (synthetic fixture)
const SYNTHETIC_PORT = Number(process.env.AWAKEN_RECORD_SYNTHETIC_PORT ?? 38088);
let syntheticDirectory;
let syntheticModel = "recording-model";
let syntheticResponsesDirectory;

function writeAnthropicEvent(response, event, data) {
  response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
}

function startAnthropicMessage(response) {
  response.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  writeAnthropicEvent(response, "message_start", {
    type: "message_start",
    message: {
      id: "msg_recording_control",
      type: "message",
      role: "assistant",
      content: [],
      model: syntheticModel,
      stop_reason: null,
      stop_sequence: null,
      usage: { input_tokens: 1, output_tokens: 1 },
    },
  });
}

function finishAnthropicMessage(response, stopReason, outputTokens = 3) {
  writeAnthropicEvent(response, "message_delta", {
    type: "message_delta",
    delta: { stop_reason: stopReason, stop_sequence: null },
    usage: { output_tokens: outputTokens },
  });
  writeAnthropicEvent(response, "message_stop", { type: "message_stop" });
  response.end();
}

function sendAnthropicText(response, text) {
  startAnthropicMessage(response);
  writeAnthropicEvent(response, "content_block_start", {
    type: "content_block_start",
    index: 0,
    content_block: { type: "text", text: "" },
  });
  writeAnthropicEvent(response, "content_block_delta", {
    type: "content_block_delta",
    index: 0,
    delta: { type: "text_delta", text },
  });
  writeAnthropicEvent(response, "content_block_stop", { type: "content_block_stop", index: 0 });
  finishAnthropicMessage(response, "end_turn", Math.max(3, Math.ceil(text.length / 4)));
}

function sendAnthropicToolUse(response, id, name, input) {
  startAnthropicMessage(response);
  writeAnthropicEvent(response, "content_block_start", {
    type: "content_block_start",
    index: 0,
    content_block: { type: "tool_use", id, name, input: {} },
  });
  writeAnthropicEvent(response, "content_block_delta", {
    type: "content_block_delta",
    index: 0,
    delta: { type: "input_json_delta", partial_json: JSON.stringify(input) },
  });
  writeAnthropicEvent(response, "content_block_stop", { type: "content_block_stop", index: 0 });
  finishAnthropicMessage(response, "tool_use", 8);
}

function controlledToolResponse(response, payloadText) {
  if (syntheticModel === "recording-repository-action-model") {
    const readId = "toolu_recording_source_read";
    const writeId = "toolu_recording_review_write";
    if (payloadText.includes(writeId)) {
      sendAnthropicText(response, "Repository checked\nThe pinned protocol source was read before any output.\n\nApproval\nOne human approval released the protected write.\n\nArtifact\nprotocol-onboarding-review.md is attached to this Session.");
      return true;
    }
    if (payloadText.includes(readId)) {
      const revision = payloadText.match(/repository revision ([0-9a-f]{12})/i)?.[1] ?? "unknown";
      const digest = payloadText.match(/source SHA-256 ([0-9a-f]{64})/i)?.[1] ?? "unknown";
      sendAnthropicToolUse(response, writeId, "write", {
        file_path: "/mnt/session/outputs/protocol-onboarding-review.md",
        content: `# Protocol onboarding verification\n\n## Source checked\nRepository revision ${revision}; source path web/src/surfaces/protocols.tsx; source SHA-256 ${digest}.\n\n## Verified path\nThe page links to awakenworks.com protocol documentation and shows an @anthropic-ai/sdk example.\n\n## Copy affordance\nThe code example includes CopyButton.\n\n## Approval boundary\nThis artifact is written only after approval; the mounted source remains read-only.\n\n## Result\nprotocol-onboarding-review.md records the verified onboarding path.\n`,
      });
      return true;
    }
    sendAnthropicToolUse(response, readId, "read", {
      file_path: "/mnt/session/uploads/mnt/files/protocols.tsx",
    });
    return true;
  }
  if (syntheticModel === "recording-restart-continuity-model") {
    const writeId = "toolu_recording_restart_write";
    if (payloadText.includes(writeId)) {
      sendAnthropicText(response, "Work resumed\nThe accepted work continued after process restart.\n\nArtifact\nrestart-continuity.md was created once.\n\nMarker\nRESTART-CONTINUITY-41");
      return true;
    }
    sendAnthropicToolUse(response, writeId, "write", {
      file_path: "/mnt/session/outputs/restart-continuity.md",
      content: "# Work item\nContinue accepted Agent work after service restart.\n\n## Accepted marker\nRESTART-CONTINUITY-41\n\n## Durable handoff\nThe original Session resumed after restart and produced this artifact once. The recovered handoff preserved marker RESTART-CONTINUITY-41.\n",
    });
    return true;
  }
  return false;
}

function recordingHostAddress() {
  if (process.env.AWAKEN_RECORD_SYNTHETIC_HOST) return process.env.AWAKEN_RECORD_SYNTHETIC_HOST;
  const candidates = Object.entries(os.networkInterfaces()).flatMap(([name, interfaces]) =>
    (interfaces ?? [])
      .filter((candidate) => candidate.family === "IPv4" && !candidate.internal)
      .map((candidate) => ({ name, address: candidate.address })),
  );
  // macOS commonly lists utun interfaces before en0. Those tunnel addresses
  // can accept neither host-side provider discovery nor Docker traffic, so a
  // first-interface policy deterministically waits for the HTTP timeout.
  const routable = candidates.find(({ name }) =>
    /^(?:en\d+|eth\d+|ens\d+|eno\d+|wlan\d+)$/.test(name),
  ) ?? candidates.find(({ name }) => !/^(?:utun|tun|tap|awdl|llw|bridge)/.test(name));
  if (routable) return routable.address;
  throw new Error("container recording fixture needs a non-loopback host address");
}

async function ensureSyntheticResponsesDirectory() {
  if (syntheticResponsesDirectory) return syntheticResponsesDirectory;
  const port = Number(process.env.AWAKEN_RECORD_RESPONSES_PORT ?? 38089);
  const server = http.createServer((request, response) => {
    const path = request.url?.split("?", 1)[0] ?? "";
    if (process.env.AWAKEN_RECORD_DEBUG_FIXTURE === "1") {
      console.log(`[record:responses-fixture] ${request.method} ${path}`);
    }
    if (request.method === "POST" && path.endsWith("/v1/responses")) {
      let body = "";
      request.on("data", (chunk) => { body += chunk; });
      request.on("end", async () => {
        const input = JSON.parse(body || "{}");
        const text = JSON.stringify(input.input ?? "").includes("CODEX ACP READY")
          ? "CODEX ACP READY"
          : "Recording fixture response";
        // Keep the real ACP transition visible long enough for viewers to see
        // pending and working feedback; an instant fixture response would make
        // the truthful progress state disappear between animation frames.
        await new Promise((resolve) => setTimeout(resolve, 1_200));
        const item = {
          id: "msg_recording_codex",
          type: "message",
          status: "completed",
          role: "assistant",
          content: [{ type: "output_text", text, annotations: [], logprobs: [] }],
        };
        const completed = {
          id: "resp_recording_codex",
          object: "response",
          created_at: Math.floor(Date.now() / 1000),
          status: "completed",
          error: null,
          incomplete_details: null,
          instructions: null,
          max_output_tokens: null,
          model: syntheticModel,
          output: [item],
          parallel_tool_calls: true,
          previous_response_id: null,
          reasoning: { effort: null, summary: null },
          store: false,
          temperature: 1,
          text: { format: { type: "text" }, verbosity: "medium" },
          tool_choice: "auto",
          tools: [],
          top_p: 1,
          truncation: "disabled",
          usage: {
            input_tokens: 1,
            input_tokens_details: { cached_tokens: 0 },
            output_tokens: 4,
            output_tokens_details: { reasoning_tokens: 0 },
            total_tokens: 5,
          },
          metadata: {},
        };
        if (input.stream !== true) {
          response.writeHead(200, { "content-type": "application/json" });
          response.end(JSON.stringify(completed));
          return;
        }
        response.writeHead(200, {
          "content-type": "text/event-stream",
          "cache-control": "no-cache",
          connection: "keep-alive",
        });
        const events = [
          { type: "response.created", response: { ...completed, status: "in_progress", output: [] } },
          { type: "response.output_item.added", output_index: 0, item: { ...item, status: "in_progress", content: [] } },
          { type: "response.content_part.added", item_id: item.id, output_index: 0, content_index: 0, part: { type: "output_text", text: "", annotations: [], logprobs: [] } },
          { type: "response.output_text.delta", item_id: item.id, output_index: 0, content_index: 0, delta: text, logprobs: [] },
          { type: "response.output_text.done", item_id: item.id, output_index: 0, content_index: 0, text, logprobs: [] },
          { type: "response.content_part.done", item_id: item.id, output_index: 0, content_index: 0, part: item.content[0] },
          { type: "response.output_item.done", output_index: 0, item },
          { type: "response.completed", response: completed },
        ];
        for (const event of events) {
          response.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
        }
        response.end("data: [DONE]\n\n");
      });
      return;
    }
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ data: [{ id: syntheticModel }], has_more: false }));
  });
  // Provider keep-alive sockets must not outlive the proof or recording that
  // owns this fixture. The browser/runtime work keeps Node alive while the case
  // is active; once it closes, stale HTTP connections cannot turn a PASS into
  // an apparently hung process.
  server.on("connection", (socket) => socket.unref());
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "0.0.0.0", resolve);
  });
  server.unref();
  syntheticResponsesDirectory = `http://${recordingHostAddress()}:${port}`;
  return syntheticResponsesDirectory;
}

async function ensureSyntheticDirectory() {
  if (syntheticDirectory) return syntheticDirectory;
  const server = http.createServer((request, response) => {
    if (request.method === "POST" && request.url?.split("?", 1)[0].endsWith("/v1/messages")) {
      let body = "";
      request.on("data", (chunk) => { body += chunk; });
      request.on("end", () => {
        if (controlledToolResponse(response, body)) return;
        startAnthropicMessage(response);
      // Session-control stories need a genuinely in-flight Anthropic response,
      // not a sleeping non-stream response that later violates the configured
      // dialect. Emit the legal stream prefix, then remain open until the real
      // operator interrupt cancels the provider request.
      response.write([
        "event: content_block_start",
        `data: ${JSON.stringify({
          type: "content_block_start",
          index: 0,
          content_block: { type: "text", text: "" },
        })}`,
        "",
        "event: content_block_delta",
        `data: ${JSON.stringify({
          type: "content_block_delta",
          index: 0,
          delta: { type: "text_delta", text: "Review in progress" },
        })}`,
        "",
        "",
      ].join("\n"));
      if (syntheticModel !== "session-control-recording-model") {
        // Metadata/provenance stories need one real realization cycle, but do
        // not claim a long-running model turn. Close their Anthropic stream
        // legally before the per-chapter fixture process exits; otherwise the
        // runtime retries a provider that has disappeared after recording.
        response.end([
          "event: content_block_stop",
          `data: ${JSON.stringify({ type: "content_block_stop", index: 0 })}`,
          "",
          "event: message_delta",
          `data: ${JSON.stringify({
            type: "message_delta",
            delta: { stop_reason: "end_turn", stop_sequence: null },
            usage: { output_tokens: 3 },
          })}`,
          "",
          "event: message_stop",
          `data: ${JSON.stringify({ type: "message_stop" })}`,
          "",
          "",
        ].join("\n"));
        return;
      }
      // Anthropic streams define `ping` as a protocol event. Use it instead of
      // an SSE comment: the current adapter treats comment-only frames as a body
      // decode failure, which would exhaust retries and make Stop run correctly
      // disable before the operator can exercise it.
      const heartbeat = setInterval(
        () => response.write('event: ping\ndata: {"type":"ping"}\n\n'),
        5_000,
      );
      heartbeat.unref();
      response.on("close", () => clearInterval(heartbeat));
      });
      return;
    }
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({
      data: [{ id: syntheticModel }],
      has_more: false,
    }));
  });
  server.on("connection", (socket) => socket.unref());
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(SYNTHETIC_PORT, "127.0.0.1", resolve);
  });
  server.unref();
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("synthetic model directory did not bind");
  syntheticDirectory = `http://127.0.0.1:${address.port}`;
  return syntheticDirectory;
}

const requestedLiveProvider = process.env.AWAKEN_RECORD_LIVE_PROVIDER?.trim();
export const LIVE_MODEL_PROVIDER = requestedLiveProvider
  || (process.env.GEMINI_PROJECT ? "vertex"
    : process.env.DEEPSEEK_API_KEY ? "deepseek"
      : process.env.OPENAI_API_KEY ? "openai"
        : "vertex");
export const LIVE_MODEL_ID = process.env.AWAKEN_RECORD_LIVE_MODEL
  ?? (LIVE_MODEL_PROVIDER === "vertex"
    ? process.env.GEMINI_MODEL ?? "gemini-2.5-flash"
    : LIVE_MODEL_PROVIDER === "deepseek" ? "deepseek-v4-flash" : "gpt-4o-mini");
export const LIVE_MODEL_LABEL = LIVE_MODEL_PROVIDER === "vertex"
  ? "Vertex AI"
  : LIVE_MODEL_PROVIDER === "deepseek" ? "DeepSeek" : "OpenAI";
export const LIVE_MODEL_AUTH = LIVE_MODEL_PROVIDER === "vertex" ? "gcloud OAuth" : "write-only API key";

function liveApiKey() {
  return process.env.AWAKEN_RECORD_LIVE_API_KEY
    ?? (LIVE_MODEL_PROVIDER === "deepseek" ? process.env.DEEPSEEK_API_KEY : process.env.OPENAI_API_KEY)
    ?? "";
}

async function recordingWorkspace(page) {
  const response = await page.request.get(`${BACKEND}/v1/config/workspace-context`);
  if (!response.ok()) {
    throw new Error(`recording Workspace context failed: ${response.status()} ${await response.text()}`);
  }
  return (await response.json()).workspace_id;
}

async function activeCredentials(page, workspaceId) {
  const response = await page.request.get(
    `${BACKEND}/v1/config/credentials?workspace_id=${encodeURIComponent(workspaceId)}`,
  );
  if (!response.ok()) {
    throw new Error(`recording credential catalog failed: ${response.status()} ${await response.text()}`);
  }
  const body = await response.json();
  if (!Array.isArray(body)) throw new Error("recording credential catalog did not return a list");
  return body;
}

/** Configure the real model through the same single Provider Connection command
 * the UI uses. Repeated stories reuse the exact active credential instead of
 * creating parallel provider secrets. */
export async function configureLiveModel(page) {
  const provider = LIVE_MODEL_PROVIDER;
  const workspaceId = await recordingWorkspace(page);
  const credentials = await activeCredentials(page, workspaceId);
  const existing = credentials.find((source) => source.provider_id === provider && source.status === "active");
  const project = process.env.GEMINI_PROJECT ?? "";
  const location = process.env.GEMINI_LOCATION ?? "global";
  const secret = liveApiKey();
  if (provider === "vertex" && !project) {
    throw new Error("Vertex runtime-effect stories require GEMINI_PROJECT and a working gcloud login");
  }
  if (provider !== "vertex" && !existing && !secret) {
    throw new Error(`${LIVE_MODEL_LABEL} runtime-effect stories require a write-only API key`);
  }
  const connectionData = {
      idempotency_key: `record-live-${provider}-v1`,
      workspace_id: workspaceId,
      provider_id: provider,
      display_name: LIVE_MODEL_LABEL,
      dialect: provider === "vertex" ? "vertex_gemini" : "open_ai_chat",
      ...(provider === "vertex" ? { configuration: { project_id: project, location } } : {}),
      ...(process.env.AWAKEN_RECORD_LIVE_BASE_URL
        ? { base_url: process.env.AWAKEN_RECORD_LIVE_BASE_URL }
        : {}),
      timeout_secs: 60,
      ...(existing
        ? { credential_source_id: existing.id }
        : provider === "vertex" ? { oauth_helper: "gcloud" } : { secret }),
  };
  let connected;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    connected = await page.request.post(`${BACKEND}/v1/config/provider-connections`, {
      data: connectionData,
    });
    if (connected.ok() || ![429, 502, 503, 504].includes(connected.status()) || attempt === 3) break;
    await delay(attempt * 1_000);
  }
  if (!connected.ok()) {
    throw new Error(
      `could not connect ${LIVE_MODEL_LABEL} through Provider Connections: ${connected.status()} ${await connected.text()}`,
    );
  }
  const connection = await connected.json();
  if (!connection.sync || connection.sync.discovered < 1) {
    throw new Error(`${LIVE_MODEL_LABEL} Provider Connection returned no discoverable models`);
  }
  const catalogResponse = await page.request.get(`${BACKEND}/v1/config/catalog`);
  if (!catalogResponse.ok()) {
    throw new Error(
      `${LIVE_MODEL_LABEL} model catalog readback failed: ${catalogResponse.status()} ${await catalogResponse.text()}`,
    );
  }
  const catalog = await catalogResponse.json();
  const activeModels = (catalog.offerings ?? [])
    .filter((offering) => offering.provider_id === provider)
    .map((offering) => offering.model_id);
  if (!activeModels.includes(LIVE_MODEL_ID)) {
    throw new Error(
      `${LIVE_MODEL_LABEL} model ${LIVE_MODEL_ID} is not an active offering; discovered: ${activeModels.join(", ") || "none"}`,
    );
  }
}

/** Deterministic provider directory for stories that do not claim live model
 * connectivity. It still enters through the exact Provider Connection command. */
export async function configureSyntheticModel(page, id) {
  syntheticModel = id;
  const provider = "anthropic";
  const directory = await ensureSyntheticDirectory();
  const workspaceId = await recordingWorkspace(page);
  const credentials = await activeCredentials(page, workspaceId);
  const existing = credentials.find(
    (source) => source.provider_id === provider && source.status === "active",
  );
  const response = await page.request.post(`${BACKEND}/v1/config/provider-connections`, {
    data: {
      idempotency_key: `record-synthetic-v2-${id}`,
      workspace_id: workspaceId,
      provider_id: provider,
      display_name: "Recording fixture",
      dialect: "anthropic_messages",
      base_url: `${directory}/v1/`,
      timeout_secs: 60,
      ...(existing ? { credential_source_id: existing.id } : { secret: SYNTHETIC_KEY }),
    },
  });
  if (!response.ok()) {
    throw new Error(`synthetic Provider Connection failed: ${response.status()} ${await response.text()}`);
  }
}

/** Deterministic OpenAI Responses endpoint used only to prove the real Codex
 * CLI/ACP/container path without spending or depending on an external model. */
export async function configureSyntheticResponsesModel(page, id) {
  syntheticModel = id;
  const provider = "openai";
  const directory = await ensureSyntheticResponsesDirectory();
  const workspaceId = await recordingWorkspace(page);
  const credentials = await activeCredentials(page, workspaceId);
  const existing = credentials.find(
    (source) => source.provider_id === provider && source.status === "active",
  );
  const response = await page.request.post(`${BACKEND}/v1/config/provider-connections`, {
    data: {
      idempotency_key: `record-synthetic-responses-v1-${id}`,
      workspace_id: workspaceId,
      provider_id: provider,
      display_name: "Codex recording fixture",
      dialect: "open_ai_responses",
      base_url: `${directory}/v1/`,
      timeout_secs: 60,
      ...(existing ? { credential_source_id: existing.id } : { secret: SYNTHETIC_KEY }),
    },
  });
  if (!response.ok()) {
    throw new Error(`synthetic Responses Provider Connection failed: ${response.status()} ${await response.text()}`);
  }
}
