// Real Codex ACP proof: select a Codex-backed environment in the console,
// create a Managed Session, and require a live Codex reply to land as one
// committed assistant message. Never release a synthetic substitute.
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, createEnvironment, publishAgent, putAgent, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = "codex-acp-agent";
const MODEL_ID = process.env.CODEX_MODEL ?? "gpt-5.6-sol";
const EXPECTED = "CODEX ACP READY";

async function readCodexCapability(page) {
  const response = await page.request.get(`${BACKEND}/v1/capabilities`);
  await requireOk(response, "Codex ACP capability readback");
  const capabilities = await response.json();
  return (capabilities.runtimes ?? []).find((runtime) => runtime.id === "acp:codex");
}

export async function prepare({ page }) {
  const deadline = Date.now() + 80_000;
  let codex;
  while (Date.now() < deadline) {
    codex = await readCodexCapability(page);
    if (codex?.local?.detected && codex.local.login_state === "available" && codex.local.negotiated) return;
    await new Promise((resolve) => setTimeout(resolve, 2_000));
  }
  throw new Error(
    `Codex ACP readiness timed out: ${codex?.local?.reason_code ?? codex?.local?.login_state ?? "not detected"}`,
  );
}

export async function run({ page, goto, checkpoint, runtimeCheckpoint, expect, click, type }) {
  if (process.env.AWAKEN_RECORD_CODEX_ACP !== "1") {
    throw new Error("set AWAKEN_RECORD_CODEX_ACP=1 and start the backend with a real Codex ACP adapter");
  }
  if (process.env.AWAKEN_RECORD_CODEX_PERSISTED_LOGIN !== "1") {
    throw new Error("set AWAKEN_RECORD_CODEX_PERSISTED_LOGIN=1 only after the local Codex login is available");
  }

  const codex = await readCodexCapability(page);
  if (!codex?.local?.detected || codex.local.login_state !== "available" || !codex.local.negotiated) {
    throw new Error(`Codex ACP is not ready: ${codex?.local?.reason_code ?? codex?.local?.login_state ?? "not detected"}`);
  }

  await putAgent(page, AGENT_ID, {
    id: AGENT_ID,
    name: "Codex ACP agent",
    // Exact-model selection belongs to the Worker-local Codex boundary. A
    // Provider-catalog Target would incorrectly require a host-side offering.
    model: { mode: "backend_exact", backend_ref: "acp:codex", model_ref: MODEL_ID },
    system: "Follow the user's concise request and keep the final answer exact.",
    tools: [], mcp_servers: [], skills: [], plugins: [], plugin_config: {},
    context_policy: { kind: "keep_all" }, max_steps: 4,
  });
  await publishAgent(page, AGENT_ID);
  const environment = await createEnvironment(page, {
    name: `Codex ACP · ${Date.now()}`,
    config: { type: "cloud" },
  }, MANAGED_HEADERS);

  await goto("/w/default/sessions");
  const newSession = page.getByRole("button", { name: /New session|新建会话/ });
  await expect(newSession).toBeVisible();
  await click(newSession);
  const modal = page.locator(".modal");
  await modal.locator("select").nth(0).selectOption(AGENT_ID);
  await modal.locator("select").nth(1).selectOption(environment.id);
  await click(modal.getByRole("button", { name: /Create|创建/, exact: true }));

  await checkpoint("the Managed Session preserves the selected Agent and Environment", async () => {
    await expect(page).toHaveURL(/\/sessions\/[^/]+$/, { timeout: 15_000 });
    const sessionId = page.url().split("/").at(-1);
    const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}`, { headers: MANAGED_HEADERS });
    await requireOk(response, "Codex ACP Session readback");
    const session = await response.json();
    expect(session.agent.id).toBe(AGENT_ID);
    expect(session.environment_id).toBe(environment.id);
  });
  await expect(page.locator(".transcript-composer")).toBeVisible();

  const composer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await type(composer, `Reply with exactly: ${EXPECTED}`, { delay: 18 });
  await composer.press("Enter");
  await expect(page.locator(".transcript-pending-message")).toContainText(EXPECTED);
  const working = page.locator(".agent-working");
  await expect(working).toBeVisible();

  const agentReply = page.locator('article[data-role="assistant"]').last();
  await runtimeCheckpoint("the real Codex ACP response commits as one coherent message", async () => {
    await expect(agentReply).toContainText(EXPECTED, { timeout: 90_000 });
    await expect(page.getByText("acp:codex", { exact: true })).toBeVisible();
    const sessionId = page.url().split("/").at(-1);
    const sessionResponse = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}`, { headers: MANAGED_HEADERS });
    await requireOk(sessionResponse, "Codex ACP Session provenance readback");
    const session = await sessionResponse.json();
    expect(session.agent.model.id).toContain(MODEL_ID);
    expect(session.agent.model.id).toContain("executor=acp:codex");
    const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, { headers: MANAGED_HEADERS });
    await requireOk(response, "Codex ACP Session events readback");
    const events = await response.json();
    const answers = events.data.filter((event) =>
      event.type === "agent.message" &&
      (event.content ?? []).some((content) => content.text?.includes(EXPECTED)));
    expect(answers).toHaveLength(1);
  });
}
