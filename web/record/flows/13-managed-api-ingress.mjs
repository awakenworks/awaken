// Managed Agents proof: create through the public wire and observe in the console.
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "managed-api-agent";
const MODEL_ID = "managed-api-recording-model";

export const story = {
  promise: "Open a Session through the Managed Agents HTTP wire and prove the console observes the same runtime object.",
  effect: "A direct POST to /v1/sessions creates an object whose Agent, title, status, and runtime provenance appear in the UI.",
  aha: "The API and console are two views of the same governed Session—integrate by wire without giving up operability.",
  loyalty: "Stable API objects protect client investment while the console keeps operations understandable over time.",
  satisfaction: "Seeing the API-created Session immediately in the UI shortens integration diagnosis and confirms ownership.",
  advocacy: "The API-to-console handoff is a clear demonstration that developers and operators share one source of truth.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, wait }) {
  await configureSyntheticModel(page, MODEL_ID);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Managed API agent", model: { id: MODEL_ID },
      system: "Serve API clients.", tools: [], mcp_servers: [], skills: [],
      plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(published.ok()).toBeTruthy();

  await goto("/w/default/sessions");
  await intro(
    "Let an existing application create Agent work through a stable public API while operators retain one console.",
    "Accept the Managed Agents Session wire and project that same object into the operational UI without a parallel silo.",
  );
  await say("The client sends one Managed Agents request; the Session remains platform-owned and observable.", 3800);
  const response = await page.request.post("http://127.0.0.1:38080/v1/sessions", {
    data: { agent: AGENT_ID, title: "Created through Managed Agents API", metadata: { source: "managed-api-video" } },
  });
  expect(response.ok()).toBeTruthy();
  const session = await response.json();
  await goto(`/w/default/sessions/${session.id}`);
  await checkpoint("the console observes the exact API-created Session", async () => {
    await expect(page.getByText(session.id, { exact: true })).toBeVisible();
    await expect(page.getByText("Created through Managed Agents API", { exact: true })).toBeVisible();
    await expect(page.getByText(AGENT_ID, { exact: true })).toBeVisible();
    const read = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${session.id}`);
    const body = await read.json();
    expect(body.metadata.source).toBe("managed-api-video");
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
