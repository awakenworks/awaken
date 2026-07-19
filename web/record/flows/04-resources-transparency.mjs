// V — "Resources & transparency." Create a memory store, bind it to an agent as a
// mounted resource (it rides into every session the agent runs), then open a real
// session and read its execution as a Trace — the run rendered as spans. Configure the
// context; inspect the internals.

const STORE = "release-memory";

export async function run({ page, goto, say, clearCaption, intro, checkpoint, aha, expect, click, type, wait }) {
  // 1) Create a memory store (a durable, mountable resource) — through the UI.
  await goto("/w/default/memory");
  await intro(
    "Give an agent durable context while keeping every mounted resource and execution step inspectable.",
    "Bind a first-class memory store, then inspect the resulting session as conversation, trace, and files.",
  );
  await click(page.getByRole("button", { name: /New memory store|新建记忆库/ }));
  await wait(400);
  await type(page.getByPlaceholder("project-memory"), STORE);
  await click(page.getByRole("button", { name: "Create", exact: true }));
  await wait(1000);
  await clearCaption();

  // 2) Bind it to an agent — mounted in every session it runs.
  await goto("/w/default/agents/release-notes-writer");
  await say("Bind the store to an agent — it mounts into every session automatically.", 4000);
  await click(page.getByRole("tab", { name: /Resources|资源/ }));
  await wait(500);
  await click(page.getByRole("button", { name: /bind a store|绑定记忆库/ }));
  await wait(400);
  await expect(page.locator("select").nth(1)).toContainText(STORE);
  await page.locator("select").nth(1).selectOption({ label: STORE });
  await type(page.getByPlaceholder("/mnt/…"), "/mnt/memory/notes");
  await click(page.getByRole("button", { name: /Save resources|保存资源/ }));
  await wait(1200);
  await checkpoint("the memory-store binding round-trips through the resource API", async () => {
    const response = await page.request.get("http://127.0.0.1:38080/v1/config/agents/release-notes-writer/resources");
    expect(response.ok()).toBeTruthy();
    const body = await response.json();
    expect(body.resources?.some((resource) => resource.kind === "memory_store")).toBeTruthy();
  });
  await clearCaption();

  // 3) Transparency: open a real session and read it as a Trace.
  await goto("/w/default/sessions");
  await say("Every run is inspectable. Open a session…", 3000);
  await click(page.locator('tr[data-click="true"]').first());
  await wait(1200);
  await say("…read the conversation, then switch to Trace.", 3200);
  await click(page.getByRole("button", { name: "Trace", exact: true }));
  await wait(1500);
  await checkpoint("the session exposes an execution trace", async () => {
    await expect(page.getByRole("button", { name: "Files", exact: true })).toBeVisible();
  });
  await say("The run rendered as spans — invoke, chat, tools. Internals, made transparent.", 4400);
  await wait(1500);
  await click(page.getByRole("button", { name: "Files", exact: true }));
  await aha("Context is not hidden prompt magic: you can trace the run and inspect exactly what the agent mounted.");
  await wait(1500);
  await clearCaption();
}
