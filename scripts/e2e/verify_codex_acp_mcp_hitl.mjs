#!/usr/bin/env node

import { spawn } from "node:child_process";
import { writeFile, readFile, mkdtemp, rm } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { createInterface } from "node:readline";

const runtime = resolve(process.env.AWAKEN_ACP_E2E_RUNTIME ?? `${process.env.HOME}/.awaken/runtime/acp-e2e`);
const requireFromRuntime = createRequire(`${runtime}/package.json`);
const importRuntime = (specifier) => import(pathToFileURL(requireFromRuntime.resolve(specifier)).href);
const [{ McpServer }, { StreamableHTTPServerTransport }, { createMcpExpressApp }, z] =
  await Promise.all([
    importRuntime("@modelcontextprotocol/sdk/server/mcp.js"),
    importRuntime("@modelcontextprotocol/sdk/server/streamableHttp.js"),
    importRuntime("@modelcontextprotocol/sdk/server/express.js"),
    importRuntime("zod/v4"),
  ]);

const root = await mkdtemp(join(tmpdir(), "awaken-codex-mcp-hitl-"));
const output = join(root, "approved.txt");
let executions = 0;
const app = createMcpExpressApp();
app.post("/mcp", async (req, res) => {
  const server = new McpServer({ name: "awaken-session-tools", version: "1" });
  server.registerTool(
    "write",
    {
      description: "Awaken-governed write. Use this instead of native filesystem operations.",
      inputSchema: { path: z.string(), content: z.string() },
    },
    async ({ path, content }) => {
      if (path !== "approved.txt" || content !== "AWAKEN_HITL_OK") {
        throw new Error("unexpected write arguments");
      }
      executions += 1;
      await writeFile(output, content, "utf8");
      return { content: [{ type: "text", text: "written" }] };
    },
  );
  const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined });
  await server.connect(transport);
  await transport.handleRequest(req, res, req.body);
  res.on("close", () => {
    transport.close();
    server.close();
  });
});
const httpServer = await new Promise((resolveServer) => {
  const listening = app.listen(0, "127.0.0.1", () => resolveServer(listening));
});
const address = httpServer.address();
const mcpUrl = `http://127.0.0.1:${address.port}/mcp`;
const executable = resolve(
  process.env.AWAKEN_CODEX_ACP ?? `${runtime}/node_modules/.bin/codex-acp`,
);

async function run(decision) {
  const child = spawn(executable, [], { stdio: ["pipe", "pipe", "pipe"] });
  const pending = new Map();
  const permissionRequests = [];
  const toolUpdates = [];
  let nextId = 1;
  let stderr = "";
  child.stderr.on("data", (chunk) => (stderr += chunk.toString()));
  const send = (message) => child.stdin.write(`${JSON.stringify(message)}\n`);
  const request = (method, params) =>
    new Promise((resolveRequest, rejectRequest) => {
      const id = nextId++;
      pending.set(id, { resolve: resolveRequest, reject: rejectRequest });
      send({ jsonrpc: "2.0", id, method, params });
    });
  createInterface({ input: child.stdout }).on("line", (line) => {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      return;
    }
    if (message.id != null && ("result" in message || "error" in message)) {
      const waiter = pending.get(message.id);
      if (!waiter) return;
      pending.delete(message.id);
      message.error ? waiter.reject(new Error(JSON.stringify(message.error))) : waiter.resolve(message.result);
      return;
    }
    if (message.method === "session/update") {
      const update = message.params?.update;
      if (["tool_call", "tool_call_update"].includes(update?.sessionUpdate)) toolUpdates.push(update);
      return;
    }
    if (message.method !== "session/request_permission" || message.id == null) return;
    permissionRequests.push(message.params);
    const kind = decision === "allow" && permissionRequests.length === 1 ? "allow_once" : "reject_once";
    const option = message.params.options.find((candidate) => candidate.kind === kind);
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: option
        ? { outcome: { outcome: "selected", optionId: option.optionId } }
        : { outcome: { outcome: "cancelled" } },
    });
  });
  const timeout = setTimeout(() => child.kill("SIGTERM"), 120_000);
  try {
    await request("initialize", {
      protocolVersion: 1,
      clientCapabilities: { fs: {}, terminal: false, auth: { terminal: false } },
      clientInfo: { name: "awaken-hitl-e2e", version: "1" },
    });
    const session = await request("session/new", {
      cwd: root,
      mcpServers: [{ type: "http", name: "awaken_session", url: mcpUrl, headers: [] }],
    });
    const result = await request("session/prompt", {
      sessionId: session.sessionId,
      prompt: [{
        type: "text",
        text: "Call the awaken_session write tool exactly once with path approved.txt and content AWAKEN_HITL_OK. Do not use native filesystem or shell tools. If rejected, stop without retrying.",
      }],
    });
    return { stopReason: result.stopReason, permissionRequests, toolUpdates };
  } catch (error) {
    throw new Error(`${decision} run failed: ${error.message}\n${stderr.slice(-3000)}`);
  } finally {
    clearTimeout(timeout);
    child.kill("SIGTERM");
  }
}

try {
  const rejected = await run("reject");
  if (executions !== 0) throw new Error("rejected tool call executed");
  const approved = await run("allow");
  const content = await readFile(output, "utf8");
  if (content !== "AWAKEN_HITL_OK" || executions !== 1) {
    throw new Error(`approved side effect mismatch: executions=${executions}`);
  }
  const subjects = [...rejected.permissionRequests, ...approved.permissionRequests].map(
    (request) => ({
      keys: Object.keys(request.toolCall ?? {}).sort(),
      title: request.toolCall?.title,
      name: request.toolCall?.name,
      kind: request.toolCall?.kind,
      inputKeys: Object.keys(request.toolCall?.rawInput ?? {}).sort(),
      metaKeys: Object.keys(request.toolCall?._meta ?? {}).sort(),
    }),
  );
  const observedCalls = [...rejected.toolUpdates, ...approved.toolUpdates]
    .filter((update) => update.sessionUpdate === "tool_call")
    .map((update) => ({
      toolCallId: update.toolCallId,
      title: update.title,
      inputKeys: Object.keys(update.rawInput ?? {}).sort(),
    }));
  process.stdout.write(`${JSON.stringify({
    mcpUrlProjected: true,
    rejected: { permissionRequests: rejected.permissionRequests.length, executions: 0 },
    approved: { permissionRequests: approved.permissionRequests.length, executions },
    observedPermissionSubjects: subjects,
    observedToolCalls: observedCalls,
    exactOnce: executions === 1,
  })}\n`);
} finally {
  await new Promise((resolveClose) => httpServer.close(resolveClose));
  await rm(root, { recursive: true, force: true });
}
