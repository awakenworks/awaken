import { expect, test, type Page } from "@playwright/test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { MANAGED_HEADERS, MEMORY_HEADERS, SKILLS_HEADERS } from "./betas";
import { SyntheticModelDirectory } from "./synthetic-model";

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
  const environmentId = (await row.locator("td").first().textContent())?.trim();
  expect(environmentId).toBeTruthy();
  const bindingResponse = await request.get(
    `/v1/awaken/environments/${environmentId}/sandbox-execution-policy`,
  );
  expect(bindingResponse.ok()).toBeTruthy();
  expect(await bindingResponse.json()).toMatchObject({
    environment_id: environmentId,
    provisioning: "on_tool_use",
    version: 1,
  });
});

test("Environment work queue: a fresh env shows its seeded healthcheck queued", async ({ page }) => {
  // Creating an environment seeds one `healthcheck` work item into its durable queue
  // (EnvRegistry + WorkQueue). The env row's Queue cell projects GET …/work/stats, so
  // a fresh env reads `depth: 1` and the cell shows "1 queued".
  const name = `envq-${Date.now()}`;
  await page.goto("/w/default/environments");
  await page.getByRole("button", { name: /New environment/ }).click();
  await page.getByPlaceholder("claude-sandbox-github").fill(name);
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
  await page.goto("/w/default/skills");
  // The backend may be reused locally and already contain a delivered Skill. Assert
  // the UI mirrors the live catalog in either state instead of assuming isolation.
  const catalog = await (await request.get("/v1/skills", { headers: SKILLS_HEADERS })).json();
  if (catalog.data.length === 0) {
    await expect(page.getByText(/No skills delivered yet|尚无已交付技能/)).toBeVisible();
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

  const catalog = await (await request.get("/v1/skills", { headers: SKILLS_HEADERS })).json();
  const skill = catalog.data.find((candidate: { display_title?: string }) => candidate.display_title === marker);
  expect(skill).toBeTruthy();
  const content = await (await request.get(`/v1/skills/${skill.id}/versions/latest/content`, { headers: SKILLS_HEADERS })).text();
  expect(content).toContain("version two");
  const latest = await (await request.get(`/v1/skills/${skill.id}/versions/latest`, { headers: SKILLS_HEADERS })).json();
  expect(latest.file_entries.find((file: { path: string }) => file.path === "helper.bin").executable).toBe(true);
});

test("Agent Resources: bind a memory store to an agent and persist it", async ({ page, request }) => {
  const store = `store-${Date.now()}`;
  const agent = `res-agent-${Date.now()}`;
  const storeResponse = await request.post("/v1/memory_stores", {
    headers: MEMORY_HEADERS,
    data: { name: store },
  });
  expect(storeResponse.ok()).toBe(true);
  const storeRecord = await storeResponse.json();
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, system: "hi", tools: [], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

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
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, ...MIN_AGENT } });

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
  const filename = `global-input-${Date.now()}.txt`;
  await page.goto("/w/default/files");
  const chooser = page.waitForEvent("filechooser");
  await page.getByRole("button", { name: /Upload file/ }).click();
  await (await chooser).setFiles({
    name: filename,
    mimeType: "text/plain",
    buffer: Buffer.from("workspace input"),
  });
  const row = page.locator("tr", { hasText: filename });
  await expect(row).toBeVisible();
  await expect(row).toContainText(/Agent input|Agent 输入/);

  const inputs = await (await request.get("/v1/files?purpose=input&limit=1000")).json();
  const artifacts = await (await request.get("/v1/files?purpose=artifact&limit=1000")).json();
  expect(inputs.data.some((file: { filename: string; purpose: string }) =>
    file.filename === filename && file.purpose === "input")).toBe(true);
  expect(artifacts.data.some((file: { filename: string }) => file.filename === filename)).toBe(false);

  await page.goto("/w/default/artifacts");
  await page.getByPlaceholder(/Filter artifacts/).fill(filename);
  await expect(page.locator("tr", { hasText: filename })).toHaveCount(0);
});

test("Agent Resources: connect a GitHub repo to an agent and persist it", async ({ page, request }) => {
  const agent = `repo-agent-${Date.now()}`;
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, ...MIN_AGENT } });
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
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, ...MIN_AGENT } });
  // Seed a skill via the multipart Skills API (the durable skill store is wired in
  // management mode, so create persists) so there's one to pick.
  const skillName = `e2e-skill-${Date.now()}`;
  await request.post("/v1/skills", {
    headers: SKILLS_HEADERS,
    multipart: {
      file: {
        name: "SKILL.md",
        mimeType: "text/markdown",
        buffer: Buffer.from("---\nname: greeter\ndescription: Say hello.\n---\n# Greeter"),
      },
      display_title: skillName,
    },
  });
  const catalog = await (await request.get("/v1/skills", { headers: SKILLS_HEADERS })).json();
  const skill = catalog.data.find((candidate: { display_title?: string; id: string }) =>
    candidate.display_title === skillName);
  expect(skill).toBeTruthy();

  await page.goto(`/w/default/agents/${agent}`);
  await openBuild(page, "Skills & MCP");
  await page.getByRole("button", { name: "+ Skill", exact: true }).click();
  await page.getByLabel("Skill id").fill(skill.id);
  await page.getByRole("button", { name: /Save draft/ }).click();
  await expect(page.locator(".ui-toast").filter({ hasText: /Saved|已保存/ })).toBeVisible();

  await page.reload();
  await openBuild(page, "Skills & MCP");
  await expect(page.getByLabel("Skill id")).toHaveValue(skill.id);
});

test("Tool presentation: alias a tool in the editor and persist it", async ({ page, request }) => {
  const agent = `tools-agent-${Date.now()}`;
  // Seed an agent with a static tool selected, so the override target picker has one.
  await request.put(`/v1/config/agents/${agent}`, { data: { id: agent, system: "hi", tools: ["read"], plugins: [], plugin_config: {}, context_policy: { kind: "keep_all" }, max_steps: 8 } });

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
  const storeId = (await (await request.post("/v1/memory_stores", {
    headers: MEMORY_HEADERS,
    data: { name: store },
  })).json()).id as string;
  // A memory_store binds at session creation (Managed Agents contract — it can't be
  // attached to a running session), so mount it via the create body's resources[].
  const sid = (await (await request.post("/v1/sessions", {
    headers: MANAGED_HEADERS,
    data: {
      agent: "default",
      title: "files-e2e",
      resources: [{ type: "memory_store", memory_store_id: storeId, mount_path: "/mnt/memory/notes" }],
    },
  })).json()).id as string;

  await page.goto(`/w/default/sessions/${sid}`);
  await page.getByRole("button", { name: "Inputs", exact: true }).click();
  await expect(page.getByText("/mnt/memory/notes")).toBeVisible(); // mounted resource path
  await expect(page.getByText(storeId)).toBeVisible(); // backing reference
  await expect(page.getByText(/creation snapshot|创建快照/)).toBeVisible();
  await page.locator(".segmented").getByRole("button", { name: "Artifacts", exact: true }).click();
  await expect(page.getByText(/No artifacts yet|还没有产物/)).toBeVisible(); // live artifacts read
});

test("Deployment: create in the UI (agent + environment) and see it listed", async ({ page, request }) => {
  const name = `dep-${Date.now()}`;
  // A deployment needs a published agent + an environment — seed both via the API.
  const agent = `dep-agent-${Date.now()}`;
  const model = `dep-model-${Date.now()}`;
  await syntheticModels.configure(request, model);
  await request.put(`/v1/config/agents/${agent}`, {
    data: {
      id: agent,
      name: agent,
      system: "hi",
      model: { id: model },
      tools: [],
      plugins: [],
      plugin_config: {},
      context_policy: { kind: "keep_all" },
      max_steps: 8,
    },
  });
  const publish = await request.post(`/v1/config/agents/${agent}/publish`);
  expect(publish.ok(), await publish.text()).toBe(true);
  await request.post("/v1/environments", {
    headers: MANAGED_HEADERS,
    data: { name: `dep-env-${Date.now()}`, config: { type: "cloud", networking: { type: "unrestricted" } } },
  });

  await page.goto("/w/default/deployments");
  await page.getByRole("button", { name: /New deployment/ }).click();
  await page.getByPlaceholder("nightly-report").fill(name);
  await page.locator("select").nth(0).selectOption({ index: 1 }); // agent
  await page.locator("select").nth(1).selectOption({ index: 1 }); // environment
  await page.getByPlaceholder("0 20 * * 5").fill("0 20 * * 5");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText(name)).toBeVisible();
});
