// Agent control-plane proof: tune the context view, compaction prompt, memory
// extraction prompts, and a state-machine reminder/continuation from one Behavior
// chapter, then verify the exact persisted configuration through the real API.

const AGENT_ID = "control-plane-demo";
const COMPACT_PROMPT = "Preserve decisions, unresolved risks, file paths, and exact verification results.";
const MEMORY_INSTRUCTIONS = "Remember durable user preferences and accepted project conventions; ignore transient logs.";
const MEMORY_TASK = "Extract only reusable facts from this completed step. Return no implementation-note memories.";
const CONTINUE_PROMPT = "Continue until the tracked work is complete: {summary}";

export async function run({ page, goto, intro, say, clearCaption, checkpoint, aha, expect, click, type, wait }) {
  await page.request.put(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`, {
    data: {
      id: AGENT_ID,
      name: "Control plane demo",
      model: { id: "" },
      system: "You are a careful project agent. Make progress, keep evidence, and finish declared work.",
      metadata: {},
      tools: [],
      mcp_servers: [],
      skills: [],
      max_steps: 8,
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
    },
  });

  await goto(`/w/default/agents/${AGENT_ID}`);
  await intro(
    "Tune what an Agent sees, remembers, compresses, and must finish without changing runtime code.",
    "The Behavior chapter keeps model context and generic runtime mechanisms together as versionable Agent configuration.",
  );

  await click(page.getByRole("tab", { name: /Behavior|行为/, exact: true }));
  await say("First bound the request view. Committed history remains intact; only model-visible context is trimmed.", 4200);
  await click(page.getByRole("button", { name: /Keep last N|保留最近 N/ }));
  await type(page.locator('input[type="number"]').first(), "24");

  const compact = page.locator(".behavior-card", { hasText: /Auto-compaction|自动压缩/ });
  await say("Compaction has its own instructions, so summaries preserve the evidence this Agent actually needs.", 4200);
  await compact.getByRole("switch").check();
  await wait(500);
  await type(compact.getByLabel("Compaction instructions", { exact: true }), COMPACT_PROMPT, { delay: 8 });

  const memory = page.locator(".behavior-card", { hasText: /Memory recall|记忆召回/ });
  await say("Memory is also per Agent: control both extractor behavior and the completed-step extraction task.", 4400);
  await memory.getByRole("switch").check();
  await wait(500);
  await type(memory.getByLabel("Memory extraction instructions", { exact: true }), MEMORY_INSTRUCTIONS, { delay: 7 });
  await type(memory.getByLabel("Extraction task prompt", { exact: true }), MEMORY_TASK, { delay: 7 });

  const machine = page.locator(".behavior-card", { hasText: /Agent behavior state machine|Agent 行为状态机/ });
  await say("A generic State Machine consumes todo facts, emits request-only reminders, and constrains completion.", 4400);
  await machine.getByRole("switch").check();
  await wait(500);
  await click(machine.getByRole("button", { name: /Todo reminder|待办提醒/ }));
  await type(machine.getByPlaceholder("max: 0"), "2");
  await type(machine.getByPlaceholder(/Continue message|继续文案/), CONTINUE_PROMPT, { delay: 8 });
  await wait(700);

  await click(page.getByRole("button", { name: /Save|保存/, exact: true }));
  await wait(1000);
  await checkpoint("one Agent persists its complete model-context and runtime behavior policy", async () => {
    const response = await page.request.get(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`);
    expect(response.ok()).toBeTruthy();
    const config = await response.json();
    expect(config.context_policy).toEqual({ kind: "keep_last", keep_last: 24 });
    expect(config.plugins).toEqual(expect.arrayContaining(["compact", "memory", "state_machine"]));
    expect(config.plugin_config.compact.instructions).toBe(COMPACT_PROMPT);
    expect(config.plugin_config.memory.instructions).toBe(MEMORY_INSTRUCTIONS);
    expect(config.plugin_config.memory.extraction_prompt).toBe(MEMORY_TASK);
    expect(config.plugin_config.state_machine.machines[0].name).toBe("todo");
    expect(config.plugin_config.state_machine.machines[0].scope).toBe("thread");
    expect(config.plugin_config.state_machine.continuation).toEqual({
      max_continuations: 2,
      message: CONTINUE_PROMPT,
    });
  });

  await clearCaption();
  await aha("One Agent page controls what the model sees, remembers, is reminded of, and is allowed to call complete.");
  await wait(1200);
  await clearCaption();
}
