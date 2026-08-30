// V — "Author an agent in plain English." Open the Admin Assistant from the Agents
// list, describe an agent, and watch it draft a FULL config — auto-picked tools plus a
// tool-description override — live via Vertex Gemini. The draft is a real unpublished agent, so
// Open-in-editor lands on the editor to review and Publish. The whole authoring loop,
// closed in the console.
import { configureLiveModel } from "../support/models.mjs";
import { BACKEND, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = `pr-reviewer-${Date.now()}`;
const ASK =
  `Draft an agent id '${AGENT_ID}' that reviews pull requests. Give it the read and grep ` +
  "tools, and rename grep to 'search_code' for the model with a helpful description.";

async function approveUntilDraft(page, openBtn, interact, wait) {
  const deadline = Date.now() + 90_000;
  let approvals = 0;
  while (Date.now() < deadline) {
    if (await openBtn.first().isVisible().catch(() => false)) return approvals;
    const allow = page.getByRole("button", { name: /^Allow$|^允许$/ }).last();
    if (await allow.isVisible().catch(() => false)) {
      await interact(allow);
      approvals += 1;
      await wait(700);
      continue;
    }
    await wait(300);
  }
  throw new Error("Assistant did not produce an operator-approved draft within 90 seconds");
}

export async function prepare({ page }) {
  await configureLiveModel(page);
}

export async function run({ page, goto, runtimeCheckpoint, expect, click, type, wait }) {
  await goto("/w/default/agents");
  await click(page.getByRole("button", { name: /Ask Assistant|询问助手/ }));
  await wait(800);

  const composer = page.getByPlaceholder(/Ask a question or describe what you want to accomplish|提问，或描述你想完成的事情/);
  await type(composer, ASK, { delay: 10 });
  await composer.press("Enter");

  // Wait for the assistant to draft (its tool calls persist a real unpublished agent,
  // which surfaces as an Open-in-editor chip below the chat).
  const openBtn = page.getByRole("button", { name: /Open in editor|在编辑器打开/ });
  await approveUntilDraft(page, openBtn, click, wait);
  await runtimeCheckpoint("the assistant persists a draft that can be opened in the editor", async () => {
    await expect(openBtn.first()).toBeVisible();
    const response = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    await requireOk(response, "AI-authored Agent readback");
    const config = await response.json();
    expect(config.tools).toEqual(expect.arrayContaining(["read", "grep"]));
    expect(config.tool_overrides).toEqual(expect.arrayContaining([expect.objectContaining({ target: "grep", alias: "search_code" })]));
  });
  await click(openBtn.first());

  // Show the auto-selected tools + the tool-description override the assistant authored.
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Tools & permissions|工具与权限/ }));

  // Publish: the human reviews the diff and ships it.
  await click(page.getByRole("button", { name: /Review & publish|审阅并发布/, exact: true }));
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
}
