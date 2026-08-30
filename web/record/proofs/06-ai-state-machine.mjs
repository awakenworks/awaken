// State Machine proof: author a state machine in plain English. The assistant does more than pick tools:
// it can compose runtime behavior. Ask for a rule ("read before you write"), inspect the
// authored machine, then deliberately violate it in a live session. The recording only
// passes when the runtime visibly blocks the write with the configured reason.
import { LIVE_MODEL_ID, configureLiveModel } from "../support/models.mjs";
import { MANAGED_HEADERS } from "../support/betas.mjs";
import { BACKEND, putAgent, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = `safe-writer-${Date.now()}`;
const DRAFT = {
  id: AGENT_ID,
  type: "agent",
  name: "Safe Writer",
  description: "A governed file Agent that enforces read-before-write per path.",
  model: { id: LIVE_MODEL_ID },
  system: "You manage files safely. Follow the user's instruction exactly and use the requested file tool.",
  tools: [{
    type: "agent_toolset_20260401",
    configs: [
      { name: "read", enabled: true, permission_policy: { type: "always_allow" } },
      { name: "write", enabled: true, permission_policy: { type: "always_allow" } },
    ],
    default_config: { enabled: false, permission_policy: { type: "always_allow" } },
  }],
  skills: [],
  mcp_servers: [],
  plugins: ["state_machine"],
  plugin_config: {
    state_machine: {
      machines: [{
        name: "read-before-write",
        scope: "thread",
        key: "{file_path}",
        initial: "unread",
        transitions: [
          { on: 'read(file_path ~ "*")', from: ["unread", "read", "written"], to: "read" },
          { on: 'write(file_path ~ "*")', from: ["read", "written"], to: "written", on_violation: { action: "deny", reason: "Read {file_path} before writing it." } },
        ],
      }],
    },
  },
};

export async function prepare({ page }) {
  await configureLiveModel(page);
  await putAgent(page, AGENT_ID, DRAFT);
}

export async function run({ page, goto, say, clearCaption, intro, runtimeCheckpoint, aha, expect, click, type, wait }) {
  await goto(`/w/default/agents/${AGENT_ID}?stage=advanced&section=orchestration`);
  await intro(
    "A rushed release request must not overwrite a file the Agent has never read.",
    "Publish read-before-write as executable policy, then ask the real model to violate it.",
  );
  await runtimeCheckpoint("the reviewable draft contains a valid lowercase read-before-write deny machine", async () => {
    const response = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    await requireOk(response, "State Machine Agent readback");
    const config = await response.json();
    const toolset = config.tools.find((tool) => tool.type === "agent_toolset_20260401");
    expect(toolset).toBeTruthy();
    expect(toolset.configs).toEqual(expect.arrayContaining([
      expect.objectContaining({ name: "read", enabled: true, permission_policy: { type: "always_allow" } }),
      expect.objectContaining({ name: "write", enabled: true, permission_policy: { type: "always_allow" } }),
    ]));
    const machines = config.plugin_config?.state_machine?.machines ?? [];
    const machine = machines.find((candidate) => candidate.name === "read-before-write");
    expect(machine).toBeTruthy();
    expect(machine.key).toBe("{file_path}");
    expect(machine.initial).toBe("unread");
    expect(machine.transitions).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ on: 'read(file_path ~ "*")', to: "read" }),
        expect.objectContaining({
          on: 'write(file_path ~ "*")',
          from: expect.arrayContaining(["read", "written"]),
          on_violation: expect.objectContaining({ action: "deny" }),
        }),
      ]),
    );
  });
  await say("Each file starts unread. Only a successful read unlocks its first write.", 3600);
  await say("Repeated writes remain valid for that file, but another unread path stays blocked.", 3800);
  await wait(2000);
  await clearCaption();

  await click(page.getByRole("button", { name: /Review & publish|审阅并发布/, exact: true }));
  await wait(900);
  await say("The exact transitions remain visible before publication.", 3200);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1500);
  await clearCaption();

  // Prove the policy in the product, not just in JSON: explicitly order the model to
  // violate it. The write tool card must show an error and expose the State Machine reason.
  await say("The test orders an immediate write without reading first.", 3600);
  await click(page.getByRole("button", { name: /Try draft|试运行草稿/ }));
  const previewSessionResponse = page.waitForResponse((response) =>
    new URL(response.url()).pathname.endsWith("/sessions") && response.request().method() === "POST",
  );
  await click(page.getByRole("button", { name: /Start preview|开始预览/ }));
  const previewSession = await previewSessionResponse;
  await requireOk(previewSession, "State Machine preview Session create");
  const sessionId = (await previewSession.json()).id;
  const ask = page.getByPlaceholder(/Ask the agent|问问这个 agent/);
  await type(
    ask,
    'Immediately write "UNSAFE" to aha.txt with the write tool. Do not read the file first.',
    { delay: 9 },
  );
  await ask.press("Enter");

  const previewTranscript = page.locator(".transcript").filter({ has: ask });
  const writeCard = previewTranscript.locator('details[data-tool="write"]').first();
  await runtimeCheckpoint("the State Machine blocks the unread write at runtime", async () => {
    await expect(writeCard).toBeVisible({ timeout: 60_000 });
    await writeCard.locator("summary").click();
    await expect(writeCard).toContainText(/blocked|Read .* before writing|denied/i, { timeout: 60_000 });
    const response = await page.request.get(`${BACKEND}/v1/sessions/${sessionId}/events`, {
      headers: MANAGED_HEADERS,
    });
    await requireOk(response, "State Machine Session events readback");
    const events = (await response.json()).data;
    const deniedWrite = events.find((event) => event.type === "agent.tool_use" && event.name === "write");
    expect(deniedWrite).toBeTruthy();
    expect(events).toEqual(expect.arrayContaining([
      expect.objectContaining({
        type: "agent.tool_result",
        tool_use_id: deniedWrite.id,
        content: expect.arrayContaining([
          expect.objectContaining({ type: "text", text: expect.stringMatching(/blocked: Read .* before writing/) }),
        ]),
      }),
    ]));
  });
  await wait(1200);
  await clearCaption();
}
