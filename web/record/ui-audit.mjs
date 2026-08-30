import { chromium } from "@playwright/test";
import { existsSync, mkdirSync, rmSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { configureSyntheticModel } from "./support/models.mjs";
import { BACKEND, createManagedSession, createOrReuseSkill, publishAgent, putAgent, requireJson, requireOk, uploadAgentFile } from "./support/control-plane.mjs";
import { FILES_HEADERS, MANAGED_HEADERS, MEMORY_HEADERS, SKILLS_HEADERS } from "./support/betas.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const state = resolve(here, "../../.recording-awaken/browser-state.json");
const origin = process.env.BACKEND_URL ?? "http://127.0.0.1:38080";
const identityMode = process.env.AWAKEN_UI_AUDIT_IDENTITY_MODE ?? "self-managed";
if (!new Set(["no-login", "self-managed"]).has(identityMode)) {
  throw new Error(`unsupported AWAKEN_UI_AUDIT_IDENTITY_MODE: ${identityMode}`);
}
const screenshotDir = process.env.AWAKEN_UI_AUDIT_SCREENSHOTS
  ? resolve(here, "audit-out")
  : "";
if (screenshotDir) {
  rmSync(screenshotDir, { recursive: true, force: true });
  mkdirSync(screenshotDir, { recursive: true });
}
if (!existsSync(state)) throw new Error("authenticated browser state is required");

const routes = [
  "overview", "sessions", "agents", "assistant", "environments", "files", "artifacts",
  "mcp", "vaults", "memory", "deployments", "skills", "models", "credentials",
  "a2a-servers", "protocols", "access", "settings", "dashboard", "audit-log",
  "datasets", "eval-runs", "agents/codex-acp-agent",
];
const viewports = [
  ["desktop", { width: 1440, height: 900 }],
  ["tablet", { width: 1024, height: 768 }],
  ["mobile", { width: 390, height: 844 }],
];

const browser = await chromium.launch({ headless: true });
const failures = [];
try {
  const setupToken = process.env.AWAKEN_RECORD_SETUP_TOKEN?.trim();
  let discovery;
  if (setupToken) {
    const handoff = await browser.newContext();
    const exchange = await handoff.request.post(`${origin}/v1/auth/local/exchange`, {
      data: { setup_token: setupToken },
    });
    if (exchange.ok()) {
      await handoff.storageState({ path: state });
      discovery = handoff;
    } else if (exchange.status() === 401) {
      await handoff.close();
    } else throw new Error(`local browser setup exchange failed: HTTP ${exchange.status()}`);
  }
  discovery ??= await browser.newContext({ storageState: state });
  const catalog = await discovery.request.get(`${origin}/v1/config/catalog`);
  if (catalog.status() === 401 && !setupToken) {
    throw new Error("local browser session expired; restart all-in-one and provide AWAKEN_RECORD_SETUP_TOKEN");
  }
  if (!catalog.ok()) throw new Error(`authenticated catalog preflight failed: HTTP ${catalog.status()}`);
  const modelId = "ui-audit-model";
  const agentId = "ui-audit-agent";
  await configureSyntheticModel(discovery, modelId);
  await putAgent(discovery, agentId, {
    id: agentId,
    name: "UI audit agent",
    model: { id: modelId },
    system: "Provide deterministic UI audit fixtures.",
    tools: [{
      type: "mcp_toolset",
      mcp_server_name: "docs",
      configs: [{ name: "search", enabled: true, permission_policy: { type: "always_allow" } }],
      default_config: { enabled: true, permission_policy: { type: "always_ask" } },
    }],
    mcp_servers: [{ type: "url", name: "docs", url: "https://docs.example.invalid/mcp" }],
    skills: [],
    plugins: [],
    plugin_config: {},
    context_policy: { kind: "keep_all" },
    max_steps: 2,
  });
  await publishAgent(discovery, agentId);
  const contextResponse = await discovery.request.get(`${origin}/v1/config/workspace-context`);
  let sessionDiscovery = { id: "", error: "", workspaceId: "", count: null };
  let sessionListUrl = `${origin}/v1/sessions?limit=1000&include_archived=true`;
  if (!contextResponse.ok()) {
    sessionDiscovery.error = `Workspace context HTTP ${contextResponse.status()}`;
  } else {
    const workspace = await contextResponse.json();
    sessionDiscovery.workspaceId = workspace.workspace_id;
    const scoped = `${origin}/v1/workspaces/${encodeURIComponent(workspace.workspace_id)}`;
    sessionListUrl = `${scoped}/sessions?limit=1000&include_archived=true`;
    const response = await discovery.request.get(`${scoped}/sessions?limit=100&include_archived=true`, {
      headers: { "anthropic-beta": "managed-agents-2026-04-01" },
    });
    if (!response.ok()) {
      sessionDiscovery.error = `Session list HTTP ${response.status()}`;
    } else {
      const body = await response.json();
      sessionDiscovery.count = Array.isArray(body.data) ? body.data.length : 0;
      // A durable test host contains historical Sessions by design. Select the
      // fixture by its business identity; list order is not fixture identity
      // and may otherwise point the audit at an unrelated Preview Session.
      const auditSession = body.data?.find((candidate) =>
        candidate.title === "UI audit session"
          && candidate.agent?.id === agentId
          && !candidate.archived_at);
      sessionDiscovery.id = auditSession?.id ?? "";
    }
  }
  if (!sessionDiscovery.id) {
    const session = await createManagedSession(discovery, {
      agent: agentId,
      title: "UI audit session",
    }, MANAGED_HEADERS);
    sessionDiscovery = {
      id: session.id,
      error: "",
      workspaceId: sessionDiscovery.workspaceId,
      count: 1,
    };
  }
  const expectedSessionInventory = await discovery.request.get(sessionListUrl, { headers: MANAGED_HEADERS });
  await requireOk(expectedSessionInventory, "UI audit Session inventory after fixture setup");
  const expectedSessionIds = new Set(((await expectedSessionInventory.json()).data ?? []).map((session) => session.id));
  const skillFixture = await createOrReuseSkill(discovery, {
    headers: SKILLS_HEADERS,
    name: "Release evidence reviewer",
    text: "---\nname: Release evidence reviewer\ndescription: Review release evidence without inventing facts.\n---\n\nReturn the decision, evidence, owner, and next action.\n",
  });
  const memoryInventory = await discovery.request.get(`${BACKEND}/v1/memory_stores`, { headers: MEMORY_HEADERS });
  await requireOk(memoryInventory, "UI audit Memory Store inventory");
  const memoryStores = (await memoryInventory.json()).data ?? [];
  const memoryFixture = memoryStores.find((store) => store.name === "Release decisions") ??
    await requireJson(await discovery.request.post(`${BACKEND}/v1/memory_stores`, {
      headers: MEMORY_HEADERS,
      data: { name: "Release decisions", description: "Reviewed release evidence and decisions." },
    }), "UI audit Memory Store create");
  const deploymentInventory = await discovery.request.get(`${BACKEND}/v1/deployments`, { headers: MANAGED_HEADERS });
  await requireOk(deploymentInventory, "UI audit Deployment inventory");
  const deployments = (await deploymentInventory.json()).data ?? [];
  const deploymentFixture = deployments.find((deployment) => deployment.name === "Release readiness review") ??
    await requireJson(await discovery.request.post(`${BACKEND}/v1/deployments`, {
      headers: MANAGED_HEADERS,
      data: {
        name: "Release readiness review",
        agent: agentId,
        environment_id: "env_local",
        schedule: { type: "cron", expression: "0 16 * * 5", timezone: "Asia/Shanghai" },
        initial_events: [{ type: "user.message", content: [{ type: "text", text: "Review the release evidence." }] }],
      },
    }), "UI audit Deployment create");
  const vaultInventory = await discovery.request.get(`${BACKEND}/v1/vaults`, { headers: MANAGED_HEADERS });
  await requireOk(vaultInventory, "UI audit Vault inventory");
  const vaults = (await vaultInventory.json()).data ?? [];
  const vaultFixture = vaults.find((vault) => vault.display_name === "Release integrations") ??
    await requireJson(await discovery.request.post(`${BACKEND}/v1/vaults`, {
      headers: MANAGED_HEADERS,
      data: { display_name: "Release integrations", metadata: { purpose: "ui-audit" } },
    }), "UI audit Vault create");
  const fileInventory = await discovery.request.get(`${BACKEND}/v1/files?limit=1000`, { headers: FILES_HEADERS });
  await requireOk(fileInventory, "UI audit File inventory");
  const matchingFiles = ((await fileInventory.json()).data ?? [])
    .filter((file) => file.filename === "release-readiness.md");
  for (const duplicate of matchingFiles.slice(1)) {
    await requireOk(await discovery.request.delete(`${BACKEND}/v1/files/${duplicate.id}`, { headers: FILES_HEADERS }), "UI audit duplicate File cleanup");
  }
  const fileFixture = matchingFiles[0] ?? await uploadAgentFile(discovery, {
    name: "release-readiness.md",
    content: "# Release readiness\n\nDecision: ready after evidence review.\nOwner: Release manager.\n",
  });
  const sessionId = sessionDiscovery.id;
  if (sessionId) routes.push(`sessions/${encodeURIComponent(sessionId)}`);
  else failures.push({
    viewport: "discovery",
    route: "session-detail",
    error: sessionDiscovery.error || "No active or archived Session was available for dynamic detail-page audit",
    workspaceId: sessionDiscovery.workspaceId,
    count: sessionDiscovery.count,
  });
  for (const [name, viewport] of viewports) {
    const page = await discovery.newPage();
    await page.setViewportSize(viewport);
    const pageErrors = [];
    const unauthorized = [];
    let currentRoute = "startup";
    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("response", (response) => {
      if (response.status() === 401) unauthorized.push({ route: currentRoute, url: response.url() });
    });
    let shellAvailable = true;
    for (const route of routes) {
      currentRoute = route;
      await page.goto(`${origin}/w/default/${route}`, { waitUntil: "domcontentloaded", timeout: 20_000 });
      await page.locator("main").waitFor({ state: "visible", timeout: 15_000 });
      // A visible shell is not a reviewed page. Wait for lazy route loading and
      // React Query gates to leave the skeleton state, then fail explicitly if
      // the current release never reaches real content.
      await page.waitForFunction(
        () => document.querySelectorAll("main .skeleton").length === 0,
        undefined,
        { timeout: 15_000 },
      ).catch(() => {});
      await page.waitForFunction(
        () => [...document.querySelectorAll("main td")].every((cell) => cell.textContent?.trim() !== "…"),
        undefined,
        { timeout: 5_000 },
      ).catch(() => {});
      await page.waitForTimeout(120);
      await page.locator(".content").evaluate((content) => { content.scrollTop = 0; });
      const layout = await page.evaluate(() => {
        const content = document.querySelector(".content");
        const shell = document.querySelector(".shell");
        const topbar = document.querySelector(".topbar");
        const sidebar = document.querySelector(".sidebar");
        const main = document.querySelector("main");
        const shellBody = document.querySelector(".shell-body");
        const bodyOverflow = document.documentElement.scrollWidth > window.innerWidth + 2;
        const badText = [...document.querySelectorAll("button, label, nav a, input, select")]
          .filter((node) => {
            if (!(node instanceof HTMLElement) || node.closest(".table-wrap, .table-scroll")) return false;
            const rect = node.getBoundingClientRect();
            const style = getComputedStyle(node);
            return rect.width > window.innerWidth + 2 ||
              (node.scrollWidth > node.clientWidth + 3 && style.overflowX === "visible" && style.whiteSpace === "nowrap");
          })
          .slice(0, 4)
          .map((node) => (node.textContent || node.getAttribute("aria-label") || node.tagName).trim().slice(0, 80));
        const rawError = [...document.querySelectorAll(".err")]
          .map((node) => node.textContent?.trim())
          .filter(Boolean)
          .filter((text) => !/not available in this deployment|此部署中不可用/i.test(text));
        const skeletons = document.querySelectorAll("main .skeleton").length;
        const loadingCells = [...document.querySelectorAll("main td")]
          .filter((cell) => cell.textContent?.trim() === "…").length;
        const clippedLongContent = !!content
          && content.scrollHeight > window.innerHeight + 2
          && content.clientHeight >= content.scrollHeight - 2;
        const rect = (node) => node?.getBoundingClientRect();
        const shellRect = rect(shell);
        const topbarRect = rect(topbar);
        const sidebarRect = rect(sidebar);
        const mainRect = rect(main);
        const shellBodyRect = rect(shellBody);
        const documentScroll = Math.max(
          Math.abs(window.scrollX),
          Math.abs(window.scrollY),
          document.documentElement.scrollHeight - document.documentElement.clientHeight,
          document.body.scrollHeight - document.body.clientHeight,
        );
        const shellEscapesViewport = !shellRect
          || Math.abs(shellRect.top) > 2
          || Math.abs(shellRect.bottom - window.innerHeight) > 2;
        const stackedNavigation = shellBody && getComputedStyle(shellBody).flexDirection === "column";
        const chromeDetached = !topbarRect || !sidebarRect || !mainRect || !shellBodyRect
          || Math.abs(topbarRect.top) > 2
          || Math.abs(topbarRect.bottom - shellBodyRect.top) > 2
          || Math.abs(mainRect.bottom - window.innerHeight) > 2
          || (stackedNavigation
            ? Math.abs(sidebarRect.bottom - mainRect.top) > 2
            : Math.abs(sidebarRect.top - mainRect.top) > 2
              || Math.abs(sidebarRect.bottom - window.innerHeight) > 2);
        return {
          bodyOverflow,
          badText,
          rawError,
          skeletons,
          loadingCells,
          clippedLongContent,
          documentScroll,
          shellEscapesViewport,
          chromeDetached,
        };
      });
      if (layout.bodyOverflow || layout.badText.length || layout.rawError.length || layout.skeletons || layout.loadingCells
        || layout.clippedLongContent || layout.documentScroll > 2 || layout.shellEscapesViewport || layout.chromeDetached) {
        failures.push({ viewport: name, route, ...layout });
      }
      if (name === "mobile" && await page.locator(".assistant-fab:visible, .assistant-fab-panel:visible").count()) {
        failures.push({ viewport: name, route, error: "Assistant overlay obscures narrow-screen content" });
      }
      if (name === "mobile" && route === "artifacts" && await page.locator(".empty-inline").count()) {
        const emptyStateWidth = await page.locator(".empty-inline").evaluate((element) => element.getBoundingClientRect().width);
        if (emptyStateWidth < 220) {
          failures.push({ viewport: name, route, error: "Table empty state collapsed into a narrow label column", emptyStateWidth });
        }
      }
      if (name === "mobile" && route === "models") {
        const labeledModelFields = await page.locator(".responsive-table-card td[data-label]").count();
        if (labeledModelFields === 0) {
          failures.push({ viewport: name, route, error: "Model catalog remained an unlabeled desktop table" });
        }
      }
      if (route.startsWith("sessions/") && /Can send message\s+no|允许发送消息\s+否/.test(await page.locator("main").innerText())) {
        const composer = page.getByLabel(/Message to agent|给 Agent 的消息/);
        const explainsBlock = await page.getByText(
          /Agent is working|Session is archived|pending tool request|cannot accept a message|Agent 正在处理|会话已归档|待审批工具|无法接收消息/,
        ).count();
        if (!await composer.isDisabled() || explainsBlock === 0) {
          failures.push({ viewport: name, route, error: "Read-only Session exposed an unexplained active composer" });
        }
      }
      if (route.startsWith("sessions/")) {
        const humanSessionHeading = page.getByRole("heading", { name: "UI audit session", exact: true });
        if (await humanSessionHeading.count() !== 1 || !await humanSessionHeading.isVisible()) {
          failures.push({ viewport: name, route, error: "Session detail did not lead with its human-readable title" });
        }
        const effectivePolicy = page.getByText(/Effective MCP policy|实际 MCP 策略/, { exact: true });
        const askByDefault = page.getByText(/ask by default|默认询问/, { exact: true });
        if (await effectivePolicy.count() !== 1 || !await effectivePolicy.isVisible()
          || await askByDefault.count() !== 1 || !await askByDefault.isVisible()) {
          failures.push({ viewport: name, route, error: "Session detail did not expose its frozen effective MCP policy" });
        }
        const firstTaskGuidance = page.getByText(/Start with the result you need|先说明你需要的结果/, { exact: true });
        if (await firstTaskGuidance.count() !== 1 || !await firstTaskGuidance.isVisible()) {
          failures.push({ viewport: name, route, error: "Empty Session did not explain how to begin" });
        }
        const composerVisibility = await page.locator(".transcript-composer").evaluate((composer) => {
          const content = document.querySelector(".content");
          const emptyState = document.querySelector(".transcript-empty-state");
          if (!content) return { visible: false, reason: "missing content scrollport" };
          const composerRect = composer.getBoundingClientRect();
          const contentRect = content.getBoundingClientRect();
          const emptyStateRect = emptyState?.getBoundingClientRect();
          return {
            visible: composerRect.top >= contentRect.top - 2 && composerRect.bottom <= contentRect.bottom + 2,
            emptyStateOverlap: !!emptyStateRect && emptyStateRect.bottom > composerRect.top + 2,
            composerTop: composerRect.top,
            composerBottom: composerRect.bottom,
            contentTop: contentRect.top,
            contentBottom: contentRect.bottom,
          };
        });
        if (!composerVisibility.visible || composerVisibility.emptyStateOverlap) {
          failures.push({
            viewport: name,
            route,
            error: !composerVisibility.visible
              ? "Session composer is outside the initial viewport"
              : "Session empty state is obscured by its composer",
            composerVisibility,
          });
        }
      }
      if (route === "skills") {
        if (await page.getByText(/Unknown|未知/, { exact: true }).count()) {
          failures.push({ viewport: name, route, error: "Skill Sandbox requirement stayed unknown" });
        }
        await page.getByRole("button", { name: /Edit|编辑/, exact: true }).first().click();
        const modal = page.locator(".modal");
        await modal.waitFor({ state: "visible", timeout: 5_000 });
        await page.waitForFunction(
          () => document.querySelector(".modal textarea")?.value.includes("Return the decision"),
          undefined,
          { timeout: 5_000 },
        ).catch(() => {});
        const editor = await modal.locator("textarea").inputValue().catch(() => "");
        if (!editor.includes("Return the decision")) {
          failures.push({ viewport: name, route, error: "Skill editor did not load the official content archive" });
        }
        if (await modal.locator(".err").count()) {
          failures.push({ viewport: name, route, error: `Skill editor failed: ${await modal.locator(".err").innerText()}` });
        }
        await modal.getByRole("button", { name: /Close|关闭/ }).click();
      }
      for (const technicalId of [
        ...(route === "overview" || route === "sessions" || route.startsWith("sessions/") ? [sessionId] : []),
        ...(route === "environments" ? ["env_local"] : []),
        ...(route === "agents/codex-acp-agent" ? ["codex-acp-agent"] : []),
        ...(route === "skills" ? [skillFixture.id] : []),
        ...(route === "memory" ? [memoryFixture.id] : []),
        ...(route === "deployments" ? [deploymentFixture.id] : []),
        ...(route === "vaults" ? [vaultFixture.id] : []),
        ...(route === "files" ? [fileFixture.id] : []),
      ]) {
        const raw = page.getByText(technicalId, { exact: true });
        if (await raw.count() && await raw.first().isVisible()) {
          failures.push({ viewport: name, route, error: `Technical identifier is primary visible copy: ${technicalId}` });
        }
      }
      // Keep exhaustive visual evidence for the two layout extremes. Tablet is
      // still exercised above, but omitting its duplicate frames limits disk use.
      if (screenshotDir && name !== "tablet") {
        await page.screenshot({
          path: resolve(screenshotDir, `${name}-${route.replaceAll("/", "-")}.png`),
          fullPage: false,
        });
        const scroll = await page.locator(".content").evaluate((content) => ({
          clientHeight: content.clientHeight,
          scrollHeight: content.scrollHeight,
        }));
        if (scroll.scrollHeight > scroll.clientHeight + 8) {
          await page.locator(".content").evaluate((content) => { content.scrollTop = content.scrollHeight; });
          await page.waitForTimeout(80);
          await page.screenshot({
            path: resolve(screenshotDir, `${name}-${route.replaceAll("/", "-")}-bottom.png`),
            fullPage: false,
          });
          await page.locator(".content").evaluate((content) => { content.scrollTop = 0; });
        }
      }
      if (await page.locator(".search-box").count() === 0) {
        failures.push({ viewport: name, route, error: "Application shell disappeared during authenticated route audit" });
        shellAvailable = false;
        break;
      }
    }
    if (shellAvailable) {
      await page.goto(`${origin}/w/default/overview`, { waitUntil: "domcontentloaded", timeout: 20_000 });
      await page.locator(".content").evaluate((content) => { content.scrollTop = content.scrollHeight; });
      await page.locator(".search-box").click();
      const search = page.getByPlaceholder(/Search pages and workflows|搜索页面与流程/);
      await search.fill("protocol");
      await page.getByRole("button", { name: /API & protocols|API 与协议/i }).click();
      await page.waitForURL(/\/w\/default\/protocols$/, { timeout: 10_000 });
      const routeScrollTop = await page.locator(".content").evaluate((content) => content.scrollTop);
      if (routeScrollTop !== 0) failures.push({ viewport: name, route: "route-scroll-reset", routeScrollTop });

      await page.locator(".identity-trigger").click();
      const identity = (await page.locator(".identity-menu").innerText()).trim();
      if (!identity || /\b[a-f0-9]{24,}\b|\b[A-Za-z0-9_-]{40,}\b/.test(identity)) {
        failures.push({ viewport: name, route: "identity-menu", identity });
      }
    }
    if (unauthorized.length) failures.push({ viewport: name, route: "unauthorized-response", unauthorized });
    if (pageErrors.length) failures.push({ viewport: name, route: "pageerror", pageErrors });
    await page.close();
  }
  const finalSessionInventory = await discovery.request.get(sessionListUrl, { headers: MANAGED_HEADERS });
  await requireOk(finalSessionInventory, "UI audit Session inventory after route review");
  const unexpectedSessionIds = ((await finalSessionInventory.json()).data ?? [])
    .map((session) => session.id)
    .filter((id) => !expectedSessionIds.has(id));
  if (unexpectedSessionIds.length) {
    failures.push({ viewport: "all", route: "navigation-write-side-effect", unexpectedSessionIds });
  }
  await discovery.close();

  const anonymous = await browser.newContext({ viewport: { width: 390, height: 844 } });
  const login = await anonymous.newPage();
  let anonymousSessionChecks = 0;
  login.on("request", (request) => {
    if (new URL(request.url()).pathname === "/v1/session") anonymousSessionChecks += 1;
  });
  await login.goto(`${origin}/w/default/overview`, { waitUntil: "domcontentloaded", timeout: 20_000 });
  if (identityMode === "self-managed") {
    await login.getByText(/LOCAL SIGN-IN|本地登录/).waitFor({ state: "visible", timeout: 15_000 });
    const body = await login.locator("body").innerText();
    if (!/Setup token|Setup Token/.test(body) || /HTTP 401|Unauthorized|Failed to fetch/.test(body)) {
      failures.push({ viewport: "mobile", route: "anonymous-sign-in", body: body.slice(0, 300) });
    }
    if (anonymousSessionChecks > 2) {
      failures.push({ viewport: "mobile", route: "anonymous-auth-loop", anonymousSessionChecks });
    }
  } else {
    await login.locator("main").waitFor({ state: "visible", timeout: 15_000 });
    const body = await login.locator("body").innerText();
    if (/LOCAL SIGN-IN|本地登录|HTTP 401|Unauthorized|Failed to fetch/.test(body)) {
      failures.push({ viewport: "mobile", route: "anonymous-no-login", body: body.slice(0, 300) });
    }
  }
  await anonymous.close();

  // Model a fully expired browser, not a stale bearer alongside a still-valid
  // self-managed cookie. A valid cookie is allowed to recover access without
  // showing sign-in; removing it makes this a real rejection -> clear -> gate
  // test instead of treating legitimate credential fallback as a failure.
  const expired = await browser.newContext({ viewport: { width: 390, height: 844 } });
  await expired.addInitScript(() => {
    sessionStorage.setItem("awaken.product.session-bearer", "expired-recording-session");
  });
  const expiredPage = await expired.newPage();
  let expiredSessionChecks = 0;
  expiredPage.on("request", (request) => {
    if (new URL(request.url()).pathname === "/v1/session") expiredSessionChecks += 1;
  });
  await expiredPage.goto(`${origin}/w/default/overview`, { waitUntil: "domcontentloaded", timeout: 20_000 });
  if (identityMode === "self-managed") {
    await expiredPage.getByText(/LOCAL SIGN-IN|本地登录/).waitFor({ state: "visible", timeout: 15_000 });
  } else {
    await expiredPage.locator("main").waitFor({ state: "visible", timeout: 15_000 });
  }
  const staleBearer = await expiredPage.evaluate(() => sessionStorage.getItem("awaken.product.session-bearer"));
  // A no-login host deliberately accepts anonymous requests, so it never emits
  // the 401 that identifies a bearer as stale. Protected mode owns the
  // rejection -> clear -> sign-in transition; do not invent that signal here.
  if (identityMode === "self-managed" && staleBearer !== null) {
    failures.push({ viewport: "mobile", route: "expired-bearer-refresh", staleBearer });
  }
  if (identityMode === "self-managed" && expiredSessionChecks > 2) {
    failures.push({ viewport: "mobile", route: "expired-auth-loop", expiredSessionChecks });
  }
  await expired.close();
} finally {
  await browser.close();
}

if (failures.length) {
  console.error(JSON.stringify(failures, null, 2));
  process.exit(1);
}
console.log(`[ui-audit] ${viewports.length} viewports × ${routes.length} routes + search, ${identityMode} identity and expired-session checks passed`);
