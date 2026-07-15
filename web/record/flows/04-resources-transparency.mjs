// V — "Resources & transparency." Create a memory store, bind it to an agent as a
// mounted resource (it rides into every session the agent runs), then open a real
// session and read its execution as a Trace — the run rendered as spans. Configure the
// context; inspect the internals.

const STORE = "release-memory";

export async function run({ page, goto, say, clearCaption, click, type, wait }) {
  // 1) Create a memory store (a durable, mountable resource) — through the UI.
  await goto("/w/default/memory");
  await say("Resources are first-class: a memory store the agent reads and writes.", 3800);
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
  await page.locator("select").nth(1).selectOption({ label: STORE }).catch(() => {});
  await type(page.getByPlaceholder("/mnt/…"), "/mnt/memory/notes");
  await click(page.getByRole("button", { name: /Save resources|保存资源/ }));
  await wait(1200);
  await clearCaption();

  // 3) Transparency: open a real session and read it as a Trace.
  await goto("/w/default/sessions");
  await say("Every run is inspectable. Open a session…", 3000);
  await click(page.locator('tr[data-click="true"]').first());
  await wait(1200);
  await say("…read the conversation, then switch to Trace.", 3200);
  await click(page.getByRole("button", { name: "Trace", exact: true }));
  await wait(1500);
  await say("The run rendered as spans — invoke, chat, tools. Internals, made transparent.", 4400);
  await wait(1500);
  await click(page.getByRole("button", { name: "Files", exact: true }));
  await say("And its Files view — the resources it mounted and the artifacts it produced.", 4200);
  await wait(1500);
  await clearCaption();
}
