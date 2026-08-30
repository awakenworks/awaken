// Tool-presentation proof: native and deferred MCP tools keep their canonical
// identity while the model receives an operator-selected alias and description.
import { configureSyntheticModel } from "../support/models.mjs";
import { BACKEND, requireOk } from "../support/control-plane.mjs";

const AGENT_ID = `tool-presentation-proof-${Date.now()}`;
const MODEL_ID = "tool-presentation-proof-model";

export async function run({ page, goto, checkpoint, expect, click, type }) {
  await configureSyntheticModel(page, MODEL_ID);
  await goto("/w/default/agents/new");
  await type(page.getByPlaceholder("coding-agent"), AGENT_ID);
  await page.getByLabel(/^Model$|^模型$/).selectOption({ label: MODEL_ID });
  await click(page.getByRole("tab", { name: /Build|构建/, exact: true }));
  await click(page.getByRole("tab", { name: /Tools & permissions|工具与权限/ }));

  const readRow = page.locator("label.check-row").filter({ hasText: "read" }).first();
  await readRow.locator('input[type="checkbox"]').check();
  await click(page.getByRole("button", { name: /override a selected tool|覆盖已选工具/ }));
  await type(page.getByLabel("Canonical tool id 1"), "read");
  await type(page.getByPlaceholder("rename"), "read_file");
  await type(page.getByPlaceholder("override description"), "Read a file from the sandbox.");

  await click(page.getByRole("button", { name: /override an MCP tool|覆盖 MCP 工具/ }));
  await type(page.getByLabel("Canonical tool id 2"), "mcp__issues__create_issue");
  await type(page.getByPlaceholder("rename").nth(1), "file_issue");
  await page.getByLabel("Show this tool to the model on demand").last().check();
  await click(page.getByRole("button", { name: /Save draft|保存草稿/, exact: true }));

  await checkpoint("native and deferred MCP tool presentation persists without changing canonical identity", async () => {
    const response = await page.request.get(`${BACKEND}/v1/config/agents/${AGENT_ID}`);
    await requireOk(response, "Tool presentation Agent readback");
    const config = await response.json();
    expect(config.tools).toEqual(["read"]);
    expect(config.mcp_servers ?? []).toEqual([]);
    expect(config.tool_overrides).toEqual(expect.arrayContaining([
      expect.objectContaining({ target: "read", alias: "read_file", description: "Read a file from the sandbox." }),
      expect.objectContaining({ target: "mcp__issues__create_issue", alias: "file_issue", exposure: "on_demand" }),
    ]));
  });
}
