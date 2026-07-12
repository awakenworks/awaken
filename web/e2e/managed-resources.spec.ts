import { expect, test } from "@playwright/test";

// Locks the console↔endpoint round-trip for the managed-resource surfaces
// (memory / skills / environments / deployments): drive the real UI against the
// real management backend and prove the create/read reaches the endpoint.

test("Memory store: create in the UI and see it listed", async ({ page }) => {
  const name = `mem-${Date.now()}`;
  await page.goto("/w/default/memory");
  await page.getByRole("button", { name: /New memory store/ }).click();
  await page.getByPlaceholder("project-memory").fill(name);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});

test("Environment: create in the UI and see it listed", async ({ page }) => {
  const name = `env-${Date.now()}`;
  await page.goto("/w/default/environments");
  await page.getByRole("button", { name: /New environment/ }).click();
  await page.getByPlaceholder("my-dev-env").fill(name);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});

test("Skills: the surface reads the delivered-skill catalog", async ({ page }) => {
  await page.goto("/w/default/skills");
  // Skills come from a durable skill store (SKILL.md), not the console — with none
  // wired the list is empty but live, proving the surface↔endpoint read works.
  await expect(page.getByText(/No skills delivered yet|尚无已交付技能/)).toBeVisible();
});

test("Agent Resources: bind a memory store to an agent and persist it", async ({ page, request }) => {
  const store = `store-${Date.now()}`;
  const agent = `res-agent-${Date.now()}`;
  await request.post("/v1/memory_stores", { data: { name: store } });
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("tab", { name: "Resources" }).click();
  await page.getByRole("button", { name: /bind a store/ }).click();
  // A memory row has three selects (kind, store, access); the store is the 2nd, and the
  // mount path is the only field carrying the `/mnt/…` placeholder.
  await page.locator("select").nth(1).selectOption({ label: store });
  const path = `/mnt/${store}`;
  await page.getByPlaceholder("/mnt/…").fill(path);
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();

  // Reload → the binding rehydrates from the stored resource config.
  await page.reload();
  await page.getByRole("tab", { name: "Resources" }).click();
  await expect(page.getByPlaceholder("/mnt/…")).toHaveValue(path);
});

const MIN_AGENT = { system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 };

test("Agent Resources: attach a file to an agent and persist it", async ({ page, request }) => {
  const agent = `file-agent-${Date.now()}`;
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, ...MIN_AGENT } });

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("tab", { name: "Resources" }).click();
  await page.getByRole("button", { name: /attach a file/ }).click();
  // Uploading is a two-step: the row's button opens a file chooser; feeding it POSTs the
  // bytes to the Files API and stamps the returned blob id (+ filename) onto the row.
  const chooser = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: /Upload/ }).click();
  await (await chooser).setFiles({ name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from("the port is 8080") });
  await expect(page.getByRole("button", { name: "notes.txt" })).toBeVisible(); // filename shown after upload
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();

  // Reload → the file binding rehydrates (label is client-only; the mount path persists).
  await page.reload();
  await page.getByRole("tab", { name: "Resources" }).click();
  await expect(page.getByPlaceholder("/mnt/…")).toHaveValue("/mnt/files/notes.txt");
});

test("Agent Resources: connect a GitHub repo to an agent and persist it", async ({ page, request }) => {
  const agent = `repo-agent-${Date.now()}`;
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, ...MIN_AGENT } });
  const url = "https://github.com/awaken/example.git";

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("tab", { name: "Resources" }).click();
  await page.getByRole("button", { name: /connect a repo/ }).click();
  await page.getByPlaceholder("https://github.com/owner/repo.git").fill(url);
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();

  await page.reload();
  await page.getByRole("tab", { name: "Resources" }).click();
  await expect(page.getByPlaceholder("https://github.com/owner/repo.git")).toHaveValue(url);
});

test("Agent Resources: add a skill to an agent and persist it", async ({ page, request }) => {
  const agent = `skill-agent-${Date.now()}`;
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, ...MIN_AGENT } });
  // Seed a skill via the multipart Skills API (the durable skill store is wired in
  // management mode, so create persists) so there's one to pick.
  await request.post("/v1/skills", {
    multipart: { file: { name: "SKILL.md", mimeType: "text/markdown", buffer: Buffer.from("# Greeter\nSay hello.") }, name: `e2e-skill-${Date.now()}` },
  });

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("tab", { name: "Resources" }).click();
  await page.getByRole("button", { name: /add a skill/ }).click();
  // A skill row: [kind][skill][mount][access] — pick the seeded skill (2nd select).
  await page.locator("select").nth(1).selectOption({ index: 0 });
  await page.getByRole("button", { name: /Save resources/ }).click();
  await expect(page.locator(".toast").filter({ hasText: /Resources saved|资源已保存/ })).toBeVisible();

  await page.reload();
  await page.getByRole("tab", { name: "Resources" }).click();
  await expect(page.getByPlaceholder("/mnt/…")).toHaveValue(/skills/);
});

test("Tool presentation: alias a tool in the editor and persist it", async ({ page, request }) => {
  const agent = `tools-agent-${Date.now()}`;
  // Seed an agent with a static tool selected, so the override target picker has one.
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, system: "hi", tools: ["read"], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

  await page.goto(`/w/default/agents/${agent}`);
  await page.getByRole("tab", { name: "Tools" }).click();
  await page.getByRole("button", { name: /override a tool/ }).click();
  // The override row: target <select> (defaults to "read"), then an "alias" input.
  await page.getByPlaceholder("rename").fill("open_file");
  await page.getByPlaceholder("override description").fill("Read a file.");
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  // Reload → the override rehydrates from the stored config.
  await page.reload();
  await page.getByRole("tab", { name: "Tools" }).click();
  await expect(page.getByPlaceholder("rename")).toHaveValue("open_file");
  await expect(page.getByPlaceholder("override description")).toHaveValue("Read a file.");
});

test("Session Files: the view projects a session's mounted resources", async ({ page, request }) => {
  // Mount a memory store on a fresh session, then prove the session's Files view
  // projects it (mounted resources) — the Anthropic Managed Agents parity view over
  // `GET /sessions/:id/resources`. Output artifacts only exist after a real run writes
  // to the outputs mount (covered by real-llm.spec harvest), so here that list is the
  // live empty-state, proving the artifacts read reached the endpoint.
  const store = `sfstore-${Date.now()}`;
  const storeId = (await (await request.post("/v1/memory_stores", { data: { name: store } })).json()).id as string;
  // A memory_store binds at session creation (Managed Agents contract — it can't be
  // attached to a running session), so mount it via the create body's resources[].
  const sid = (await (await request.post("/v1/sessions", {
    data: {
      agent: "default",
      title: "files-e2e",
      resources: [{ type: "memory_store", memory_store_id: storeId, mount_path: "/mnt/memory/notes" }],
    },
  })).json()).id as string;

  await page.goto(`/w/default/sessions/${sid}`);
  await page.getByRole("button", { name: "Files", exact: true }).click();
  await expect(page.getByText("/mnt/memory/notes")).toBeVisible(); // mounted resource path
  await expect(page.getByText(storeId)).toBeVisible(); // backing reference
  await expect(page.getByText(/No artifacts yet|还没有产物/)).toBeVisible(); // live artifacts read
});

test("Deployment: create in the UI (agent + environment) and see it listed", async ({ page, request }) => {
  const name = `dep-${Date.now()}`;
  // A deployment needs a published agent + an environment — seed both via the API.
  const agent = `dep-agent-${Date.now()}`;
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, name: agent, system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });
  await request.post(`/v1/config/agents/${agent}/publish`);
  await request.post("/v1/environments", { data: { name: `dep-env-${Date.now()}`, config: { type: "cloud", networking: { type: "unrestricted" } } } });

  await page.goto("/w/default/deployments");
  await page.getByRole("button", { name: /New deployment/ }).click();
  await page.getByPlaceholder("nightly-report").fill(name);
  await page.locator("select").nth(0).selectOption({ index: 1 }); // agent
  await page.locator("select").nth(1).selectOption({ index: 1 }); // environment
  await page.getByPlaceholder("0 20 * * 5").fill("0 20 * * 5");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});
