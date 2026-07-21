// Skill proof: a concise user goal activates delivered procedural guidance.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";

const AGENT_ID = "skill-briefing-agent";
const SKILL_TEXT = `---
name: release-signal
description: Use when the user asks for the release signal.
---
# Release signal

When the user asks for the release signal, answer with exactly: SKILL READY
Do not add punctuation or explanation.`;

export const story = {
  promise: "Give an Agent a reusable Skill, then prove a two-word goal produces the exact specialized result.",
  effect: "The Skill is delivered as a mounted resource and the live model answers the concise request with SKILL READY.",
  aha: "The user states the goal; the delivered Skill carries the procedure and makes the Agent immediately useful.",
  loyalty: "Reusable procedural knowledge makes every later Agent cheaper to create and more consistent to operate.",
  satisfaction: "A short request produces a verified specialist result without forcing the user to repeat instructions.",
  advocacy: "The contrast between a two-word request and a precise result is compact, repeatable, and easy to share.",
};

export async function run({ page, goto, intro, say, clearCaption, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  await configureLiveModel(page);
  const skillResponse = await page.request.post("http://127.0.0.1:38080/v1/skills", {
    multipart: {
      file: { name: "SKILL.md", mimeType: "text/markdown", buffer: Buffer.from(SKILL_TEXT) },
      name: "release-signal",
    },
  });
  expect(skillResponse.ok()).toBeTruthy();
  const skill = await skillResponse.json();
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID,
      name: "Skill briefing agent",
      model: { id: LIVE_MODEL_ID },
      system: "Use the delivered Skill that matches the user's goal. Keep the response concise.",
      tools: [], mcp_servers: [], skills: [{ id: skill.id }], plugins: [], plugin_config: {},
      context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(published.ok()).toBeTruthy();

  await goto(`/w/default/agents/${AGENT_ID}`);
  await intro(
    "Let users state only the outcome while reusable expertise supplies the procedure.",
    "Deliver a versioned Skill to the Agent, mount it in every Session, and verify its effect with the real model.",
  );
  await click(page.getByRole("tab", { name: /Integrations|集成/, exact: true }));
  await say("The Agent references a versioned Skill id; the procedure is discovered and activated only when the goal matches.", 4000);
  await checkpoint("the published Agent carries the delivered Skill binding", async () => {
    await expect(page.getByLabel("Skill id")).toHaveValue(skill.id);
    const response = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    const config = await response.json();
    expect(config.skills).toEqual([{ id: skill.id }]);
  });

  await say("Now give the Agent only the goal. The Skill owns the procedural detail.", 3400);
  await click(page.getByRole("button", { name: /Try it|试运行/ }));
  await click(page.getByRole("button", { name: /Start session|开始会话/ }));
  const sessionId = (await page.locator("code").filter({ hasText: /^sesn_/ }).last().innerText()).trim();
  const composer = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(composer, "Release signal", { delay: 18 });
  await composer.press("Enter");
  const agentReply = page.locator(".card").filter({ hasText: "⬡ agent" }).last();
  await runtimeCheckpoint("the real Agent applies the Skill to the concise goal", async () => {
    await expect(agentReply).toContainText("SKILL READY", { timeout: 60_000 });
    const response = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${sessionId}/events`);
    const events = await response.json();
    expect(events.data.some((event) => event.type === "agent.tool_use" && event.name === "Skill")).toBeTruthy();
  });
  await clearCaption();
  await aha(story.aha);
  await wait(900);
  await clearCaption();
}
