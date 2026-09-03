#!/usr/bin/env node

import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const executable = resolve(
  process.env.AWAKEN_CODEX_ACP ??
    `${process.env.HOME}/.awaken/runtime/acp-e2e/node_modules/.bin/codex-acp`,
);
await access(executable);

const cwd = await mkdtemp(join(tmpdir(), "awaken-codex-acp-e2e-"));
const output = join(cwd, "cwd-proof.txt");
const child = spawn(executable, [], { stdio: ["pipe", "pipe", "pipe"] });
const pending = new Map();
const updates = [];
let nextId = 1;

const send = (message) => child.stdin.write(`${JSON.stringify(message)}\n`);
const request = (method, params) =>
  new Promise((resolveRequest, rejectRequest) => {
    const id = nextId++;
    pending.set(id, { resolve: resolveRequest, reject: rejectRequest });
    send({ jsonrpc: "2.0", id, method, params });
  });

createInterface({ input: child.stdout }).on("line", async (line) => {
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
    if (message.error) waiter.reject(new Error(JSON.stringify(message.error)));
    else waiter.resolve(message.result);
    return;
  }
  if (message.method === "session/update") {
    updates.push(message.params?.update);
    return;
  }
  if (message.id == null) return;

  if (message.method === "session/request_permission") {
    const allow = message.params?.options?.find((option) => option.kind === "allow_once");
    send({
      jsonrpc: "2.0",
      id: message.id,
      result: allow
        ? { outcome: { outcome: "selected", optionId: allow.optionId } }
        : { outcome: { outcome: "cancelled" } },
    });
    return;
  }
  send({
    jsonrpc: "2.0",
    id: message.id,
    error: { code: -32601, message: `Unsupported E2E client method: ${message.method}` },
  });
});

let stderr = "";
child.stderr.on("data", (chunk) => {
  stderr += chunk.toString();
});

const timeout = setTimeout(() => {
  child.kill("SIGTERM");
}, 120_000);

try {
  const initialized = await request("initialize", {
    protocolVersion: 1,
    clientCapabilities: {
      fs: { readTextFile: false, writeTextFile: false },
      terminal: false,
      auth: { terminal: false },
    },
    clientInfo: { name: "awaken-e2e", title: "Awaken ACP E2E", version: "1" },
  });
  const session = await request("session/new", { cwd, mcpServers: [] });
  const prompt = await request("session/prompt", {
    sessionId: session.sessionId,
    prompt: [
      {
        type: "text",
        text: "Create cwd-proof.txt in the current working directory with exactly AWAKEN_ACP_CWD_OK and then reply DONE.",
      },
    ],
  });
  const content = await readFile(output, "utf8");
  if (content.trimEnd() !== "AWAKEN_ACP_CWD_OK") {
    throw new Error(`unexpected file content: ${JSON.stringify(content)}`);
  }
  const toolUpdates = updates.filter((update) =>
    ["tool_call", "tool_call_update"].includes(update?.sessionUpdate),
  );
  process.stdout.write(
    `${JSON.stringify({
      protocolVersion: initialized.protocolVersion,
      agent: initialized.agentInfo?.name,
      mcpCapabilities: initialized.agentCapabilities?.mcpCapabilities,
      sessionId: session.sessionId,
      stopReason: prompt.stopReason,
      cwdHonored: true,
      toolUpdateCount: toolUpdates.length,
    })}\n`,
  );
} catch (error) {
  process.stderr.write(`${error.stack ?? error}\n${stderr.slice(-4000)}`);
  process.exitCode = 1;
} finally {
  clearTimeout(timeout);
  child.kill("SIGTERM");
  await rm(cwd, { recursive: true, force: true });
}
