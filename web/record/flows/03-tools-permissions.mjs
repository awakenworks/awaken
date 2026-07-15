// V — "Tools, presentation, and the permission gate." Configure what an agent CAN
// do and how it's allowed to do it: pick tools from the host catalog, rename/redescribe
// one for the model (tool presentation), then gate calls with a default decision + an
// ordered deny rule. Pure configurability — no code, enforced at runtime.

const AGENT_ID = "file-ops-agent";
const SYSTEM = "You are a careful file-operations assistant. Prefer read-only actions.";

export async function run({ page, goto, say, clearCaption, click, type, wait, cursorTo, tap }) {
  await page.request.delete(`http://127.0.0.1:38080/v1/config/agents/${AGENT_ID}`).catch(() => {});

  await goto("/w/default/agents/new");
  await say("What an agent CAN do is configuration — pick tools, shape them, gate them.", 4200);
  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await click(page.locator("select").first());
  await page.locator("select").first().selectOption("kimi-k2-0711-preview");
  await type(page.locator("textarea").first(), SYSTEM, { delay: 12 });

  // Tools section.
  await click(page.getByRole("tab", { name: /Tools|工具/ }));
  await wait(500);
  await say("Pick tools from the host's advertised catalog — or add an MCP tool id.", 3800);
  for (const t of ["bash", "read", "write"]) {
    const row = page.locator("label.check-row").filter({ hasText: t }).first();
    await cursorTo(row);
    await tap();
    await row.locator('input[type="checkbox"]').check();
    await wait(350);
  }
  await wait(400);

  // Tool presentation: alias + description override.
  await say("Tool presentation: rename a tool for the model, or override its description.", 4200);
  await click(page.getByRole("button", { name: /override a tool|覆盖一个工具/ }));
  await wait(400);
  await type(page.getByPlaceholder("rename"), "run_shell");
  await type(page.getByPlaceholder("override description"), "Run a shell command in the sandbox.");
  await wait(400);

  // Permission gate.
  const perm = page.locator(".permission-editor");
  await say("Then gate every call. Default to Ask — a human approves before it runs.", 4200);
  await perm.locator(".field").filter({ hasText: /Default decision|默认裁决/ }).getByRole("button", { name: /^Ask|询问/ }).click();
  await wait(500);
  await say("And add a hard rule: shell deletes are always denied. Deny always wins.", 4200);
  await click(perm.getByRole("button", { name: /add rule|添加规则/ }));
  await type(perm.getByPlaceholder("Bash(*rm*)"), "Bash(*rm*)");
  const ruleRow = perm.locator(".row").filter({ has: page.getByPlaceholder("Bash(*rm*)") });
  await cursorTo(ruleRow.getByRole("button", { name: /^Deny|拒绝/ }));
  await tap();
  await ruleRow.getByRole("button", { name: /^Deny|拒绝/ }).click();
  await wait(600);
  await clearCaption();

  // Persist.
  await click(page.getByRole("button", { name: "Save", exact: true }));
  await wait(1000);
  await say("Save, then Publish — the gate compiles into the agent's runtime config.", 4000);
  await click(page.getByRole("button", { name: /Publish/ }));
  await wait(900);
  await click(page.locator(".modal").getByRole("button", { name: /Publish/ }));
  await wait(1200);
  await say("Tools chosen, presented, and gated — all declarative, all versioned.", 4000);
  await clearCaption();
}
