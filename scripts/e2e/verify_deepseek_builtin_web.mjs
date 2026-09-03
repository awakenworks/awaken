#!/usr/bin/env node

import { createDecipheriv } from "node:crypto";
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const dataDir = process.env.AWAKEN_DATA_DIR ?? `${homedir()}/.awaken`;
const database = `${dataDir}/credential.db`;
const keyPath = `${dataDir}/control-seal.key`;

async function sqlite(query) {
  const { stdout } = await execFileAsync("sqlite3", ["-json", database, query], {
    maxBuffer: 1024 * 1024,
  });
  return JSON.parse(stdout || "[]");
}

const sources = await sqlite(
  "SELECT data FROM credential_source WHERE data LIKE '%deepseek-official%' AND data LIKE '%\"status\":\"active\"%' LIMIT 1",
);
if (sources.length !== 1) {
  throw new Error("no active DeepSeek credential exists in the Awaken Vault");
}
const source = JSON.parse(sources[0].data);
const escapedRef = source.material_ref.replaceAll("'", "''");
const rows = await sqlite(
  `SELECT hex(sealed) AS sealed_hex FROM credential_secret WHERE secret_ref='${escapedRef}'`,
);
if (rows.length !== 1) throw new Error("DeepSeek Vault material is missing");

const key = Buffer.from((await readFile(keyPath, "utf8")).trim(), "hex");
const sealed = Buffer.from(rows[0].sealed_hex, "hex");
const nonce = sealed.subarray(0, 12);
const ciphertextAndTag = sealed.subarray(12);
const ciphertext = ciphertextAndTag.subarray(0, -16);
const tag = ciphertextAndTag.subarray(-16);
const decipher = createDecipheriv("chacha20-poly1305", key, nonce, {
  authTagLength: 16,
});
decipher.setAuthTag(tag);
const apiKey = Buffer.concat([decipher.update(ciphertext), decipher.final()]).toString("utf8");

async function checkedJson(url, init) {
  const response = await fetch(url, init);
  const body = await response.json();
  if (!response.ok) {
    throw new Error(`${url} returned HTTP ${response.status}: ${JSON.stringify(body)}`);
  }
  return body;
}

const responsesModel = process.env.AWAKEN_DEEPSEEK_RESPONSES_MODEL ?? "deepseek-v4-flash";
const responses = await checkedJson("https://api.deepseek.com/v1/responses", {
  method: "POST",
  headers: { authorization: `Bearer ${apiKey}`, "content-type": "application/json" },
  body: JSON.stringify({
    model: responsesModel,
    input: "Search the web for the current official DeepSeek API documentation URL and cite it.",
    tools: [{ type: "web_search" }],
    tool_choice: { type: "web_search" },
    store: false,
  }),
});
const responsesSearch = responses.output?.some((item) => item.type === "web_search_call") === true;
if (!responsesSearch) throw new Error("DeepSeek Responses returned no typed web_search_call");

const anthropicModel = process.env.AWAKEN_ANTHROPIC_MODEL ?? "deepseek-v4-pro";
const anthropic = await checkedJson("https://api.deepseek.com/anthropic/v1/messages", {
  method: "POST",
  headers: {
    "x-api-key": apiKey,
    "anthropic-version": "2023-06-01",
    "content-type": "application/json",
  },
  body: JSON.stringify({
    model: anthropicModel,
    max_tokens: 1024,
    messages: [
      {
        role: "user",
        content: "Use web search to find the current official DeepSeek API documentation URL and cite it.",
      },
    ],
    tools: [{ type: "web_search_20250305", name: "web_search" }],
  }),
});
const anthropicSearch =
  anthropic.content?.some((item) =>
    ["server_tool_use", "web_search_tool_result"].includes(item.type),
  ) === true;
if (!anthropicSearch) throw new Error("DeepSeek Anthropic returned no hosted-search typed block");

process.stdout.write(
  `${JSON.stringify({
    credentialSource: source.id,
    secretPrinted: false,
    responses: { model: responsesModel, typedWebSearch: responsesSearch },
    anthropic: { model: anthropicModel, typedWebSearch: anthropicSearch },
  })}\n`,
);
