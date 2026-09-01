import { expect, test, type Page } from "@playwright/test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { FILES_HEADERS, MANAGED_HEADERS, MEMORY_HEADERS, SKILLS_HEADERS } from "./betas";
import { SyntheticModelDirectory } from "./synthetic-model";
import { logicalApiPath, workspaceApiPath } from "./workspace";
import { skillFilesFromTar } from "../src/surfaces/skills";
import { typedAgentTools } from "../test-support/agent-tools.mjs";

const syntheticModels = new SyntheticModelDirectory();

test.beforeAll(async () => {
  await syntheticModels.start();
});

test.afterAll(async () => {
  await syntheticModels.stop();
});

async function openBuild(page: Page, section: "Tools & permissions" | "Skills & MCP" | "Memory & resources") {
  await page.getByRole("tab", { name: "Build", exact: true }).click();
  await page.getByRole("tab", { name: section, exact: true }).click();
}

// Locks the console↔endpoint round-trip for the managed-resource surfaces
// (memory / skills / environments / deployments): drive the real UI against the
// real management backend and prove the create/read reaches the endpoint.

test("Memory store: create in the UI and see it listed", async ({ page, request }) => {
  const name = `mem-${Date.now()}`;
  const description = `Browser-created ${name}`;
  await page.goto("/w/default/memory");
  await page.getByRole("button", { name: /New memory store/ }).click();
  await page.getByPlaceholder("project-memory").fill(name);
  await page.getByPlaceholder("What this store remembers").fill(description);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.locator("tr", { hasText: description })).toBeVisible();
  const catalogResponse = await request.get(await workspaceApiPath(request, "/v1/memory_stores"), {
    headers: MEMORY_HEADERS,
  });
  expect(catalogResponse.ok(), await catalogResponse.text()).toBe(true);
  const catalog = await catalogResponse.json();
  expect(catalog.data.some((store: { name: string; description?: string }) =>
    store.name === name && store.description === description)).toBe(true);
});

test("Environment: create in the UI and see it listed", async ({ page }) => {
  const name = `env-${Date.now()}`;
  await page.goto("/w/default/environments");
  await page.getByRole("button", { name: /New environment/ }).click();
  await page.getByPlaceholder("claude-sandbox-github").fill(name);
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});

test("Environment: configure native Sandbox creation on the first Hand tool", async ({ page, request }) => {
  const name = `lazy-env-${Date.now()}`;
  await page.goto("/w/default/environments");
  await page.getByRole("button", { name: /New environment/ }).click();
  await page.getByPlaceholder("claude-sandbox-github").fill(name);
  await page.getByRole("button", { name: /self-hosted|自管/ }).click();
  await page.getByRole("button", { name: /on first Hand tool|首次 Hand 工具时/ }).click();
  await page.getByRole("button", { name: "Create", exact: true }).click();

  const row = page.locator("tr", { hasText: name });
  await expect(row).toContainText(/first Hand tool|首次 Hand 工具/);
  const environmentId = (await row.locator("td").first().locator("code").textContent())?.trim();
  expect(environmentId).toBeTruthy();
  const bindingResponse = await request.get(
    await workspaceApiPath(request, `/v1/awaken/environments/${environmentId}/sandbox-execution-policy`),
  );
  expect(bindingResponse.ok()).toBeTruthy();
  expect(await bindingResponse.json()).toMatchObject({
    environment_id: environmentId,
    provisioning: "on_tool_use",
    version: 1,
  });
});

test("Environment work queue: a fresh self-hosted env shows its seeded healthcheck queued", async ({ page }) => {
  // Environment queue decision table: C1=self-hosted placement, C2=create
  // succeeds. C1+C2 seeds one durable healthcheck and the Queue projection shows
  // `1 queued`; !C1+C2 is cloud-provider owned and remains idle. This case covers
  // the only branch that owns a Worker work queue instead of conflating both.
  const name = `envq-${Date.now()}`;
  await page.goto("/w/default/environments");
  await page.getByRole("button", { name: /New environment/ }).click();
  await page.getByPlaceholder("claude-sandbox-github").fill(name);
  await page.getByRole("button", { name: /self-hosted|自管/ }).click();
  await page.getByRole("button", { name: "Create", exact: true }).click();
  const row = page.locator("tr", { hasText: name });
  await expect(row).toContainText(/1 queued|1 排队/);
});

test("Models exposes Provider Connections as the single authoring path", async ({ page }) => {
  await page.goto("/w/default/models");
  await expect(page.getByRole("heading", { name: /Provider connections|Provider 连接/ })).toBeVisible();
  await expect(page.getByRole("button", { name: "Author", exact: true })).toHaveCount(0);
  await expect(page.getByPlaceholder("model-id")).toHaveCount(0);
});

test("Skills: the surface reads the delivered-skill catalog", async ({ page, request }) => {
  // Catalog projection table: an empty authoritative catalog renders the one
  // create/import empty state; a non-empty catalog renders its first durable id.
  // Neither branch invents a client-side Skill index.
  await page.goto("/w/default/skills");
  // The backend may be reused locally and already contain a delivered Skill. Assert
  // the UI mirrors the live catalog in either state instead of assuming isolation.
  const catalog = await (await request.get(await workspaceApiPath(request, "/v1/skills"), { headers: SKILLS_HEADERS })).json();
  if (catalog.data.length === 0) {
    await expect(page.getByText(/No Skills yet|还没有技能/)).toBeVisible();
  } else {
    await expect(page.locator("tr", { hasText: catalog.data[0].id }).first()).toBeVisible();
  }
});

test("Skills: import a bundle and publish an online edit as a new version", async ({ page, request }) => {
  // UI cause/effect rules: C1 a selected local root SKILL.md -> E1 one durable
  // catalog row; C2 edit latest text and publish -> E2 append v2 (never mutate
  // v1), E3 the version content endpoint returns the edit, and C3 an executable
  // toggle -> E4 the new immutable version retains that bit. Backend decision
  // rules separately cover ZIP/folder canonicalization, binary retention, and
  // stale If-Match; this scenario owns the browser→real HTTP round trip.
  const marker = `ui-skill-${Date.now()}`;
  await page.goto("/w/default/skills");
  await page.getByRole("button", { name: /Import Skill|导入技能/ }).click();
  await page.getByLabel(/Display title|显示名称/).fill(marker);
  const folder = page.locator('input[type="file"]').first();
  const skillDir = await mkdtemp(join(tmpdir(), "awaken-ui-skill-"));
  await writeFile(join(skillDir, "SKILL.md"), `---\nname: ${marker}\ndescription: browser import\n---\nversion one`);
  await writeFile(join(skillDir, "helper.bin"), new Uint8Array([0, 159, 255]));
  await folder.setInputFiles(skillDir);
  await page.getByRole("button", { name: /Import|导入/, exact: true }).click();
  await expect(page.getByRole("dialog", { name: /Import Skill|导入技能/ })).toBeHidden();
  await rm(skillDir, { recursive: true, force: true });
  const row = page.locator("tr", { hasText: marker });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: /Edit|编辑/ }).click();
  await page.getByRole("button", { name: "helper.bin", exact: true }).click();
  await page.getByLabel(/Executable script|可执行脚本/).check();
  await page.getByRole("button", { name: "SKILL.md", exact: true }).click();
  const editor = page.locator("textarea");
  await expect(editor).toHaveValue(/version one/);
  await editor.fill(`---\nname: ${marker}\ndescription: browser import\n---\nversion two`);
  await page.getByRole("button", { name: /Publish new version|发布新版本/ }).click();
  await expect(row).toContainText("2");

  const catalog = await (await request.get(await workspaceApiPath(request, "/v1/skills"), { headers: SKILLS_HEADERS })).json();
  const skill = catalog.data.find((candidate: { display_title?: string }) => candidate.display_title === marker);
  expect(skill).toBeTruthy();
  const archiveResponse = await request.get(await workspaceApiPath(request, `/v1/skills/${skill.id}/versions/latest/content`), { headers: SKILLS_HEADERS });
  expect(archiveResponse.ok(), await archiveResponse.text()).toBe(true);
  const archiveEntries = skillFilesFromTar(new Uint8Array(await archiveResponse.body()));
  expect(new TextDecoder().decode(archiveEntries.find((file) => file.path === "SKILL.md")?.bytes)).toContain("version two");
  expect(archiveEntries.find((file) => file.path === "helper.bin")?.executable).toBe(true);
});

test("Agent Resources: bind a memory store to an agent and persist it", async ({ page, request }) => {
  const store = `store-${Date.now()}`;
  const agent = `res-agent-${Date.now()}`;
  const storeResponse = await request.post(await workspaceApiPath(request, "/v1/memory_stores"), {
    headers: MEMORY_HEADERS,
    data: { name: store },
  });
  expect(storeResponse.ok()).toBe(true);
  const storeRecord = await storeResponse.json();
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), { data: { id: agent, system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

  await page.goto(`/w/default/agents/${agent}`);
  await openBuild(page, "Memory & resources");
  await page.getByRole("button", { name: /bind a store/ }).click();
  // A memory row has three selects (kind, store, access); the store is the 2nd, and the
  // mount path is the only field carrying the `/mnt/…` placeholder.
  await page.getByLabel("Store").selectOption(storeRecord.id);
  const path = `/mnt/${store}`;
  await page.getByPlaceholder("/mnt/…").fill(path);
  await page.getByRole("button", { name: /Save draft/ }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  // Reload → the binding rehydrates from the stored resource config.
  await page.reload();
  await openBuild(page, "Memory & resources");
  await expect(page.getByPlaceholder("/mnt/…")).toHaveValue(path);
});

const MIN_AGENT = { system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 };

test("Agent Resources: attach a file to an agent and persist it", async ({ page, request }) => {
  const agent = `file-agent-${Date.now()}`;
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), { data: { id: agent, ...MIN_AGENT } });

  await page.goto(`/w/default/agents/${agent}`);
  await openBuild(page, "Memory & resources");
  await page.getByRole("button", { name: /attach a file/ }).click();
  // Uploading is a two-step: the row's button opens a file chooser; feeding it POSTs the
  // bytes to the Files API and stamps the returned blob id (+ filename) onto the row.
  const chooser = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: /Upload/ }).click();
  await (await chooser).setFiles({ name: "notes.txt", mimeType: "text/plain", buffer: Buffer.from("the port is 8080") });
  await expect(page.getByRole("button", { name: "notes.txt" })).toBeVisible(); // filename shown after upload
  await page.getByRole("button", { name: /Save draft/ }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  // Reload → the file binding rehydrates (label is client-only; the mount path persists).
  await page.reload();
  await openBuild(page, "Memory & resources");
  await expect(page.getByPlaceholder("/mnt/…")).toHaveValue("/mnt/files/notes.txt");
});

test("Files: upload an input through the global resource library and keep it out of Artifacts", async ({ page, request }) => {
  // File view decision table: workspace inputs have no Session scope; artifacts
  // carry a Session scope. The API exposes one inventory and the two Console
  // surfaces project it by scope, so the test must not invent a `purpose` query.
  const filename = `global-input-${Date.now()}.txt`;
  await page.goto("/w/default/files");
  const chooser = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: /Upload file/ }).click();
  await (await chooser).setFiles({
    name: filename,
    mimeType: "text/plain",
    buffer: Buffer.from("workspace input"),
  });
  await page.getByRole("button", { name: /List|列表/, exact: true }).click();
  const row = page.locator("tr", { hasText: filename });
  await expect(row).toBeVisible();
  await expect(row).toContainText(/Agent input|Agent 输入/);

  const inventoryResponse = await request.get(await workspaceApiPath(request, "/v1/files?limit=1000"), { headers: FILES_HEADERS });
  expect(inventoryResponse.ok(), await inventoryResponse.text()).toBe(true);
  const inventory = await inventoryResponse.json();
  expect(inventory.data.some((file: { filename: string; scope?: unknown }) =>
    file.filename === filename && file.scope == null)).toBe(true);
  expect(inventory.data.some((file: { filename: string; scope?: unknown }) =>
    file.filename === filename && file.scope != null)).toBe(false);

  await page.goto("/w/default/artifacts");
  await page.getByRole("button", { name: /List|列表/, exact: true }).click();
  await page.getByPlaceholder(/Filter artifacts/).fill(filename);
  await expect(page.locator("tr", { hasText: filename })).toHaveCount(0);
});

test("Agent Resources: connect a GitHub repo to an agent and persist it", async ({ page, request }) => {
  const agent = `repo-agent-${Date.now()}`;
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), { data: { id: agent, ...MIN_AGENT } });
  const repositoryId = `repo-${Date.now()}`;

  await page.goto(`/w/default/agents/${agent}`);
  await openBuild(page, "Memory & resources");
  await page.getByRole("button", { name: /connect a repo/ }).click();
  await page.getByPlaceholder("repo-…").fill(repositoryId);
  await page.getByRole("button", { name: /Save draft/ }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  await page.reload();
  await openBuild(page, "Memory & resources");
  await expect(page.getByPlaceholder("repo-…")).toHaveValue(repositoryId);
});

test("Agent Resources: add a skill to an agent and persist it", async ({ page, request }) => {
  const agent = `skill-agent-${Date.now()}`;
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), { data: { id: agent, ...MIN_AGENT } });
  // Seed a skill via the multipart Skills API (the durable skill store is wired in
  // management mode, so create persists) so there's one to pick.
  const skillName = `e2e-skill-${Date.now()}`;
  const createSkill = await request.post(await workspaceApiPath(request, "/v1/skills"), {
    headers: SKILLS_HEADERS,
    multipart: {
      file: {
        name: "SKILL.md",
        mimeType: "text/markdown",
        buffer: Buffer.from(`---\nname: ${skillName}\ndescription: Say hello.\n---\n# Greeter`),
      },
      display_title: skillName,
    },
  });
  expect(createSkill.ok(), await createSkill.text()).toBe(true);
  const skill = await createSkill.json() as { id: string; display_title?: string };
  expect(skill.display_title).toBe(skillName);

  const hydrated = page.waitForResponse((response) =>
    response.request().method() === "GET"
      && logicalApiPath(response.url()) === `/v1/config/agents/${agent}`);
  await page.goto(`/w/default/agents/${agent}`);
  expect((await hydrated).ok()).toBe(true);
  await openBuild(page, "Skills & MCP");
  await page.getByRole("button", { name: "+ Skill", exact: true }).click();
  // Skill-binding decision table: an id present in the authoritative catalog is
  // selected from that catalog; an absent id is retained only while rehydrating
  // an older binding. New bindings must not recreate the removed free-text path.
  const skillBindings = page.locator(".agent-config-card", { hasText: "Skill bindings" });
  await skillBindings.locator("select").selectOption(skill.id);
  await page.getByRole("button", { name: /Save draft/ }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  await page.reload();
  await openBuild(page, "Skills & MCP");
  await expect(page.locator(".agent-config-card", { hasText: "Skill bindings" }).locator("select"))
    .toHaveValue(skill.id);
});

test("Tool presentation: alias a tool in the editor and persist it", async ({ page, request }) => {
  const agent = `tools-agent-${Date.now()}`;
  // Tool-presentation decision rule: C1 canonical Agent ToolSet enables `read`
  // and C2 the override targets that member. Effects: E1 Managed validation has
  // one tool-policy authority and E2 the editor persists the alias for `read`.
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), { data: { id: agent, system: "hi", tools: typedAgentTools(["read"]), plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

  await page.goto(`/w/default/agents/${agent}`);
  await openBuild(page, "Tools & permissions");
  await page.getByRole("button", { name: /override a selected tool/ }).click();
  // The override row: canonical target input defaults to "read", followed by alias.
  await page.getByPlaceholder("rename").fill("open_file");
  await page.getByPlaceholder("override description").fill("Read a file.");
  await page.getByRole("button", { name: "Save draft", exact: true }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  // Reload → the override rehydrates from the stored config.
  await page.reload();
  await openBuild(page, "Tools & permissions");
  await expect(page.getByPlaceholder("rename")).toHaveValue("open_file");
  await expect(page.getByPlaceholder("override description")).toHaveValue("Read a file.");
});

test("Session Inputs and Artifacts are separate backend projections", async ({ page, request }) => {
  // Mount a memory store on a fresh session, then prove the session's Files view
  // projects it (mounted resources) — the Anthropic Managed Agents parity view over
  // `GET /sessions/:id/resources`. Output artifacts only exist after a real run writes
  // to the outputs mount (covered by real-llm.spec harvest), so here that list is the
  // live empty-state, proving the artifacts read reached the endpoint.
  const store = `sfstore-${Date.now()}`;
  const storeId = (await (await request.post(await workspaceApiPath(request, "/v1/memory_stores"), {
    headers: MEMORY_HEADERS,
    data: { name: store },
  })).json()).id as string;
  const stamp = Date.now();
  const agent = `session-files-agent-${stamp}`;
  const model = `session-files-model-${stamp}`;
  await syntheticModels.configure(request, model, await workspaceApiPath(request, "/v1/config"));
  const authored = await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), {
    data: { id: agent, model: { id: model }, ...MIN_AGENT },
  });
  expect(authored.ok(), await authored.text()).toBe(true);
  const published = await request.post(await workspaceApiPath(request, `/v1/config/agents/${agent}/publish`));
  expect(published.ok(), await published.text()).toBe(true);
  // A memory_store binds at session creation (Managed Agents contract — it can't be
  // attached to a running session), so mount it via the create body's resources[].
  const sessionResponse = await request.post(await workspaceApiPath(request, "/v1/sessions"), {
    headers: MANAGED_HEADERS,
    data: {
      agent,
      environment_id: "env_local",
      title: "files-e2e",
      resources: [{ type: "memory_store", memory_store_id: storeId }],
    },
  });
  expect(sessionResponse.ok(), await sessionResponse.text()).toBe(true);
  const session = await sessionResponse.json();
  const sid = session.id as string;
  const derivedMountPath = session.resources[0]?.mount_path as string;
  expect(derivedMountPath).toBe(`/mnt/memory/${store}`);

  // Resource projection decision table: create+no Worker effect leaves the
  // id-only Memory input at the catalog-derived path and must not label it
  // mounted; a successful claimed run activates the exact frozen generation,
  // after which Inputs and resource retrieval preserve that returned path/id.
  // Artifacts remain independently empty until output bytes are published.
  const run = await request.post(await workspaceApiPath(request, `/v1/sessions/${sid}/events`), {
    headers: MANAGED_HEADERS,
    data: { events: [{ type: "user.message", content: [{ type: "text", text: "activate inputs" }] }] },
  });
  expect(run.ok(), await run.text()).toBe(true);

  await page.goto(`/w/default/sessions/${sid}`);
  await page.getByRole("button", { name: "Inputs", exact: true }).click();
  await expect(page.getByText(derivedMountPath)).toBeVisible(); // mounted resource path
  await expect(page.getByRole("main").getByText("Memory", { exact: true })).toBeVisible();
  const resourcesResponse = await request.get(await workspaceApiPath(request, `/v1/sessions/${sid}/resources`), {
    headers: MANAGED_HEADERS,
  });
  expect(resourcesResponse.ok(), await resourcesResponse.text()).toBe(true);
  const resources = await resourcesResponse.json();
  expect(resources.data).toContainEqual(expect.objectContaining({
    type: "memory_store",
    memory_store_id: storeId,
    mount_path: derivedMountPath,
  }));
  await expect(page.getByText(/stay the same for the life|本次 Session 中保持不变/)).toBeVisible();
  await page.locator(".segmented").getByRole("button", { name: "Artifacts", exact: true }).click();
  await expect(page.getByText(/No artifacts yet|还没有产物/)).toBeVisible(); // live artifacts read
});

test("Deployment: create in the UI, run once, and open its resulting Session", async ({ page, request }) => {
  const name = `Scheduled maintenance ${Date.now()}`;
  // A deployment needs a published agent + an environment — seed both via the API.
  const agent = `dep-agent-${Date.now()}`;
  const model = `dep-model-${Date.now()}`;
  await syntheticModels.configure(request, model, await workspaceApiPath(request, "/v1/config"));
  await request.put(await workspaceApiPath(request, `/v1/config/agents/${agent}`), {
    data: {
      id: agent,
      name: "Scheduled maintenance agent",
      system: "hi",
      model: { id: model },
      tools: [],
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });
  const publish = await request.post(await workspaceApiPath(request, `/v1/config/agents/${agent}/publish`));
  expect(publish.ok(), await publish.text()).toBe(true);
  const environment = await request.post(await workspaceApiPath(request, "/v1/environments"), {
    headers: MANAGED_HEADERS,
    data: { name: "Scheduled maintenance environment", config: { type: "cloud", networking: { type: "unrestricted" } } },
  });
  expect(environment.ok(), await environment.text()).toBe(true);
  const environmentId = (await environment.json()).id as string;

  await page.goto("/w/default/deployments");
  await page.getByRole("button", { name: /New deployment/ }).click();
  const modal = page.locator(".modal");
  await modal.getByPlaceholder("nightly-report").fill(name);
  await modal.getByLabel("Agent", { exact: true }).selectOption(agent);
  await modal.getByLabel("Environment", { exact: true }).selectOption(environmentId);
  await modal.getByPlaceholder("0 20 * * 5").fill("0 20 * * 5");
  await modal.getByRole("button", { name: "Create", exact: true }).click();
  const row = page.locator("tr", { hasText: name });
  await expect(row).toBeVisible();
  await expect(row.getByRole("button", { name: "Run once", exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: "Pause", exact: true })).toBeVisible();
  await row.getByRole("button", { name: "Run once", exact: true }).click();
  await expect(page.getByText("Deployment run created.")).toBeVisible();
  await page.getByRole("link", { name: "Open Session", exact: true }).click();
  await expect(page).toHaveURL(/\/w\/default\/sessions\/[^/]+$/);
  await expect(page.getByText("Run the scheduled task.", { exact: true })).toBeVisible();
});
