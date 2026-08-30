// Skill proof: a concise user goal activates delivered procedural guidance.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS, SKILLS_HEADERS } from "../support/betas.mjs";
import { BACKEND, createOrReuseSkill, publishAgent, putAgent, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = "skill-briefing-agent";
const SKILL_TEXT = `---
name: release-signal
description: Use when the user asks for the release signal.
---
# Release signal

When the user asks for the release signal, answer with exactly: SKILL READY
Do not add punctuation or explanation.`;

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, checkpoint, runtimeCheckpoint, expect, click, type }) {
  const skill = await createOrReuseSkill(page, { headers: SKILLS_HEADERS, name: "release-signal", text: SKILL_TEXT });
  await putAgent(page, AGENT_ID, {
      id: AGENT_ID,
      name: "Skill briefing agent",
      model: { id: LIVE_MODEL_ID },
      system: "When a relevant Skill is listed in your context, read its SKILL.md with the read tool, then follow its instructions exactly.",
      tools: [{
        type: "agent_toolset_20260401",
        configs: [{ name: "read", enabled: true, permission_policy: { type: "always_allow" } }],
        default_config: { enabled: false, permission_policy: { type: "always_ask" } },
      }], mcp_servers: [], skills: [{ id: skill.id }], plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  await publishAgent(page, AGENT_ID);

  await goto(`/w/default/agents/${AGENT_ID}`);
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Skills & MCP|Skills 与 MCP/, exact: true }));
  await checkpoint("the published Agent carries the delivered Skill binding", async () => {
    await expect(page.getByLabel("Skill", { exact: true })).toHaveValue(skill.id);
    const response = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    await requireOk(response, "Skill Agent readback");
    const config = await response.json();
    expect(config.skills).toEqual([{ type: "custom", skill_id: skill.id, version: "latest" }]);
  });

  await click(page.getByRole("button", { name: /Try draft|试运行草稿/ }));
  const previewSessionResponse = page.waitForResponse((response) =>
    new URL(response.url()).pathname.endsWith("/sessions") && response.request().method() === "POST",
  );
  await click(page.getByRole("button", { name: /Start preview|开始预览/ }));
  const previewSession = await previewSessionResponse;
  await requireOk(previewSession, "Skill preview Session create");
  const sessionId = (await previewSession.json()).id;
  const composer = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(composer, "Release signal", { delay: 18 });
  await composer.press("Enter");
  await runtimeCheckpoint("the real Agent applies the Skill to the concise goal", async () => {
    await expect(page.getByText("SKILL READY", { exact: true })).toBeVisible({ timeout: 90_000 });
    const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
    await requireOk(response, "Skill Session events readback");
    const events = await response.json();
    expect(events.data.some((event) => event.type === "agent.tool_use" && event.name === "read")).toBeTruthy();
  });
}
