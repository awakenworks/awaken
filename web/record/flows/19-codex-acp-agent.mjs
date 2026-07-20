// Real Codex ACP proof: select a Codex-backed environment in the console, create
// a Managed Session, and require a live Codex reply to land as one committed
// assistant message. Never release a synthetic substitute for this story.
import { spawnSync } from "node:child_process";
import { statSync } from "node:fs";
import { configureSyntheticModel } from "../support/models.mjs";

const AGENT_ID = "codex-acp-agent";
const MODEL_ID = process.env.CODEX_MODEL ?? "gpt-5.6-sol";
const EXPECTED = "CODEX ACP READY";

function managedContainerIds(expect) {
  const result = spawnSync("docker", ["ps", "-q", "--filter", "label=awaken.sandbox=1"], { encoding: "utf8" });
  expect(result.status, `Docker is required for the Codex ACP recording: ${result.stderr || result.error || ""}`).toBe(0);
  return new Set(result.stdout.trim().split(/\s+/).filter(Boolean));
}

function inspectCredentialContainer(id) {
  const result = spawnSync("docker", ["inspect", id], { encoding: "utf8" });
  if (result.status !== 0) return null;
  const [container] = JSON.parse(result.stdout);
  const env = container.Config?.Env ?? [];
  const credentialMount = (container.Mounts ?? []).find((mount) => mount.Destination === "/acp-config");
  const access = spawnSync(
    "docker",
    ["exec", id, "sh", "-c", "test -r /acp-config/auth.json && test -w /acp-config/auth.json"],
    { encoding: "utf8" },
  );
  return {
    user: container.Config?.User,
    nativeHome: env.includes("CODEX_HOME=/acp-config"),
    apiKeyAbsent: !env.some((entry) => entry.startsWith("OPENAI_API_KEY=")),
    writableCredential: credentialMount?.RW === true && access.status === 0,
  };
}

export const story = {
  promise: "Run a portable Agent through the real Codex CLI while keeping Session control and evidence in Awaken.",
  effect: "A Managed Session visibly selects acp:codex, shows immediate progress, and commits one complete live Codex reply.",
  aha: "The Agent stayed portable while Codex became its managed execution engine—and the proof returned to one transcript.",
  loyalty: "Replaceable execution engines protect Agent investment while durable Sessions keep operational history familiar.",
  satisfaction: "Visible startup progress and one coherent reply remove the uncertainty normally hidden behind a CLI adapter.",
  advocacy: "Watching the same Agent switch to a real Codex engine without integration code is a concise portability proof.",
};

export async function run({ page, goto, intro, say, focus, beat, clearCaption, checkpoint, runtimeCheckpoint, aha, expect, click, type, wait }) {
  if (process.env.AWAKEN_RECORD_CODEX_ACP !== "1") {
    throw new Error("set AWAKEN_RECORD_CODEX_ACP=1 and start the backend with a real Codex ACP adapter");
  }
  if (process.env.AWAKEN_RECORD_CODEX_ACP_CONTAINER !== "1") {
    throw new Error("set AWAKEN_RECORD_CODEX_ACP_CONTAINER=1 and run the backend with AWAKEN_SANDBOX_TIER=docker");
  }
  const credentialFile = process.env.AWAKEN_ACP_CREDENTIAL_FILE;
  if (!credentialFile || (statSync(credentialFile).mode & 0o777) !== 0o600) {
    throw new Error("AWAKEN_ACP_CREDENTIAL_FILE must name the operator-selected 0600 Codex auth.json");
  }

  // The model record satisfies the protocol-neutral Agent schema. Execution is
  // owned by the selected acp:codex environment and must produce the live proof.
  await configureSyntheticModel(page, MODEL_ID);
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID, name: "Codex ACP agent", model: { id: MODEL_ID },
      system: "Follow the user's concise request and keep the final answer exact.",
      tools: [], mcp_servers: [], skills: [], plugins: [], plugin_config: {},
      context_policy: { kind: "keep_all" }, max_steps: 4,
    },
  });
  const published = await page.request.post(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}/publish`);
  expect(published.ok()).toBeTruthy();
  const environmentResponse = await page.request.post("http://127.0.0.1:38080/v1/environments", {
    data: { name: `Codex ACP · ${Date.now()}`, config: { type: "self_hosted", runtime: "acp:codex" } },
  });
  expect(environmentResponse.ok()).toBeTruthy();
  const environment = await environmentResponse.json();

  await goto("/w/default/sessions");
  const newSession = page.getByRole("button", { name: /New session|新建会话/ });
  await focus(newSession);
  await intro(
    "Use Codex as the execution engine without rewriting a reusable Agent or losing operational evidence.",
    "Select a Codex ACP environment, run it through Managed Agents, and commit the live CLI result to the same Session transcript.",
  );

  await say("Create a Session from the portable Agent and choose Codex only at the execution boundary.", 3600);
  await click(newSession);
  const modal = page.locator(".modal");
  await modal.locator("select").nth(0).selectOption(AGENT_ID);
  await modal.locator("select").nth(1).selectOption(environment.id);
  await click(modal.getByRole("button", { name: /Create|创建/, exact: true }));

  await checkpoint("the Managed Session carries Codex ACP runtime provenance", async () => {
    await expect(page).toHaveURL(/\/sessions\/[^/]+$/, { timeout: 15_000 });
    await expect(page.getByText("acp:codex", { exact: true })).toBeVisible();
    const sessionId = page.url().split("/").at(-1);
    const response = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${sessionId}`);
    const session = await response.json();
    expect(session.agent.id).toBe(AGENT_ID);
    expect(session.environment_id).toBe(environment.id);
    expect(session.metadata["awaken.runtime"]).toBe("acp:codex");
  });
  await beat(
    "The Session records acp:codex as runtime provenance; the Agent itself remains protocol-neutral.",
    page.getByText("acp:codex", { exact: true }),
    3400,
  );

  const composer = page.getByLabel(/Message to agent|给 Agent 的消息/);
  await say("Send one decisive request. Awaken exposes progress immediately while the Codex adapter initializes.", 3600);
  await type(composer, `Reply with exactly: ${EXPECTED}`, { delay: 18 });
  const baselineContainers = managedContainerIds(expect);
  const observedContainers = new Set();
  const credentialProofs = [];
  const containerProbe = setInterval(() => {
    for (const id of managedContainerIds(expect)) {
      if (!baselineContainers.has(id) && !observedContainers.has(id)) {
        observedContainers.add(id);
        const proof = inspectCredentialContainer(id);
        if (proof) credentialProofs.push(proof);
      }
    }
  }, 100);
  await composer.press("Enter");
  await expect(page.locator(".transcript-pending-message")).toContainText(EXPECTED);
  const working = page.locator(".agent-working");
  await expect(working).toBeVisible();
  await say("Codex is running through ACP; the animated status keeps the wait visible instead of looking stalled.", 3600);
  if (await working.isVisible()) {
    await say("ACP streams progress live; Awaken will commit the completed response as one readable message.", 3200);
  }

  const agentReply = page.locator(".card").filter({ hasText: "⬡ agent" }).last();
  try {
    await runtimeCheckpoint("the containerized Codex ACP response commits as one coherent message", async () => {
      await expect(agentReply).toContainText(EXPECTED, { timeout: 90_000 });
      await expect(page.locator(".transcript-pending-message")).toContainText(/sent ✓|已发送 ✓/);
      expect(observedContainers.size, "a new awaken.sandbox Docker container must execute the run").toBeGreaterThan(0);
      expect(
        credentialProofs.some((proof) =>
          proof.user === "10001" && proof.nativeHome && proof.apiKeyAbsent && proof.writableCredential),
        "Codex must run non-root with a writable native auth.json and no OPENAI_API_KEY env",
      ).toBeTruthy();
      const sessionId = page.url().split("/").at(-1);
      const response = await page.request.get(`http://127.0.0.1:38080/v1/sessions/${sessionId}/events`);
      const events = await response.json();
      const answers = events.data.filter((event) =>
        event.type === "agent.message" &&
        (event.content ?? []).some((content) => content.text?.includes(EXPECTED)));
      expect(answers).toHaveLength(1);
    });
  } finally {
    clearInterval(containerProbe);
  }

  await clearCaption();
  await aha(story.aha);
  await wait(1000);
  await clearCaption();
}
