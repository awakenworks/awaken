import { FILES_HEADERS } from "./betas.mjs";

const DEFAULT_BACKEND = "http://127.0.0.1:38080";

// BACKEND_URL is the recording harness authority. Keeping a second, unrelated
// default here once let the browser operate one all-in-one instance while a
// checkpoint silently proved a different one. AWAKEN_HTTP_URL remains a
// backwards-compatible fallback for standalone support-module consumers.
export const BACKEND = (
  process.env.BACKEND_URL ?? process.env.AWAKEN_HTTP_URL ?? DEFAULT_BACKEND
).replace(/\/$/, "");

export async function requireOk(response, label) {
  if (!response.ok()) {
    throw new Error(`${label} failed: HTTP ${response.status()} ${await response.text()}`);
  }
  return response;
}

export async function requireJson(response, label) {
  await requireOk(response, label);
  return response.json();
}

export async function putAgent(page, id, data) {
  return requireJson(
    await page.request.put(`${BACKEND}/v1/config/agents/${id}`, { data }),
    `Agent ${id} draft upsert`,
  );
}

export async function publishAgent(page, id) {
  return requireJson(
    await page.request.post(`${BACKEND}/v1/config/agents/${id}/publish`),
    `Agent ${id} publication`,
  );
}

export async function putAgentResources(page, id, inputs) {
  const current = await page.request.get(`${BACKEND}/v1/config/agents/${id}/resources`);
  let revision = 1;
  if (current.ok()) {
    revision = Number((await current.json()).revision ?? 0) + 1;
  } else if (current.status() !== 404) {
    await requireOk(current, `Agent ${id} Resource read`);
  }
  return requireJson(
    await page.request.put(`${BACKEND}/v1/config/agents/${id}/resources`, {
      data: { agent_id: id, inputs, revision },
    }),
    `Agent ${id} Resource CAS`,
  );
}

export async function uploadAgentFile(page, { name, content, mimeType = "text/plain" }) {
  return requireJson(
    await page.request.post(`${BACKEND}/v1/files`, {
      headers: FILES_HEADERS,
      multipart: {
        file: { name, mimeType, buffer: Buffer.from(content) },
      },
    }),
    `Agent input File ${name} upload`,
  );
}

export async function createManagedSession(page, data, headers = {}) {
  return requireJson(
    await page.request.post(`${BACKEND}/v1/sessions`, {
      headers,
      data: { environment_id: "env_local", ...data },
    }),
    "Managed Session create",
  );
}

export async function sendManagedEvents(page, sessionId, events, headers = {}) {
  return requireJson(
    await page.request.post(`${BACKEND}/v1/sessions/${sessionId}/events`, {
      headers,
      data: { events },
    }),
    `Managed Session ${sessionId} Event delivery`,
  );
}

export async function createEnvironment(page, data, headers = {}) {
  return requireJson(
    await page.request.post(`${BACKEND}/v1/environments`, { headers, data }),
    "Managed Environment create",
  );
}

export async function createOrReuseSkill(page, { headers = {}, name, text }) {
  // The Skills wire accepts `display_title` (beta) / `display_name` (GA).
  // An earlier fixture sent an ignored `name` field, so recordings showed an
  // opaque generated id even though their scripts supplied a human title.
  const response = await page.request.post(`${BACKEND}/v1/skills`, {
    headers,
    multipart: {
      file: { name: "SKILL.md", mimeType: "text/markdown", buffer: Buffer.from(text) },
      display_title: name,
    },
  });
  if (response.ok()) return response.json();
  if (response.status() !== 409) return requireJson(response, `Skill ${name} delivery`);
  const conflict = await response.json();
  const details = [conflict.error, conflict.error?.message, conflict.message]
    .filter((value) => typeof value === "string")
    .join("\n");
  const id = details.match(/skill `(skill_[^`]+)`/)?.[1];
  if (!id) {
    throw new Error(`Skill ${name} conflict did not identify the reusable Skill: ${JSON.stringify(conflict)}`);
  }
  return { id };
}
