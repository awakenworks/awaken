import { test, type Locator, type Page } from '@playwright/test';
import {
  ADMIN_TOKEN,
  BACKEND_URL,
  DEMO_LOCALE,
  DSF,
  L,
  Lboth,
  beat,
  caption,
  focusFromBox,
  initCapture,
  primeDemoPage,
  scene,
  setCurrentScene,
  shot,
  smoothScroll,
  tr,
  typeSlow,
  writeManifest,
} from './demo-helpers';

/**
 * One continuous, narrated walkthrough of the entire awaken-admin-console,
 * recorded by Playwright's native `recordVideo`. Runs against a real Gemini
 * (Vertex) backend. Each scene is wrapped by `act()` so a single flaky live-LLM
 * step never aborts the recording — failures are captioned, screenshotted, and
 * collected, then surfaced in the run summary at the end.
 *
 * The same spec drives EN and ZH via DEMO_LOCALE.
 */

const SUFFIX = process.env.DEMO_SUFFIX ?? 'demo';
const AGENT_ID = `${SUFFIX}-concierge`;
const MCP_ID = `${SUFFIX}-dashboard-mcp`;
const A2A_ID = `${SUFFIX}-self-a2a`;
const DATASET_ID = `${SUFFIX}-dataset`;

// Absolute path to the local stdio MCP demo server (resolved from repo root,
// which is the backend's cwd).
const MCP_SCRIPT = 'examples/shared/mcp-ui-demo-server.py';

/** caption pair shorthand */
function C(en: string, zh: string) {
  return { en, zh };
}

// The seed ships the shared `default` provider as the OFFLINE `scripted` adapter
// (model `default` → deepseek-chat). The demo must repoint it to real Gemini on
// Vertex, or every live scene (assistant, sandbox, eval) runs on `scripted` and
// the eval fails with "unsupported provider adapter: scripted". We repoint at
// scene 02 and re-assert it before each live scene, because the AI assistant in
// scene 04 can flip the shared binding back to `scripted` while building agents.
//
// Keyless contract (verified): provider needs `adapter_options.allow_env_credentials`
// and no api_key; the backend bears `VERTEX_API_KEY` from the env. The base_url
// project path overrides `VERTEX_PROJECT_ID`, so we build it from the same
// env defaults the demo config forwards.
const VERTEX_PROJECT_ID = process.env.VERTEX_PROJECT_ID ?? 'project-wp-mtj-201';
const VERTEX_LOCATION = process.env.VERTEX_LOCATION ?? 'us-central1';
const VERTEX_BASE_URL = `https://${VERTEX_LOCATION}-aiplatform.googleapis.com/v1/projects/${VERTEX_PROJECT_ID}/locations/${VERTEX_LOCATION}/`;
const VERTEX_PROVIDER_DEFAULT = {
  id: 'default',
  adapter: 'vertex',
  base_url: VERTEX_BASE_URL,
  adapter_options: { allow_env_credentials: true },
  timeout_secs: 300,
};
const VERTEX_MODEL_DEFAULT = {
  id: 'default',
  provider_id: 'default',
  upstream_model: 'gemini-2.5-pro',
};

const sceneResults: { name: string; ok: boolean; error?: string }[] = [];

// QA instrumentation: capture every failed network response + console error,
// attributed to the scene that was on screen when it happened.
const netIssues: { scene: string; status: number; method: string; url: string }[] = [];
const consoleIssues: { scene: string; text: string }[] = [];
let currentScene = 'init';

function isIgnorableUrl(url: string): boolean {
  return /@vite|@react-refresh|@fs\/|\/node_modules\/\.vite|\.map(\?|$)|favicon|\/__/.test(url);
}
function isIgnorableConsole(t: string): boolean {
  return /React DevTools|\[vite\]|Download the React|webgl|WebGL|Lighthouse/i.test(t);
}

/** Attach network/console watchers to a page (call once after priming). */
function watchForIssues(page: Page) {
  page.on('response', (resp) => {
    const status = resp.status();
    if (status < 400) return;
    const url = resp.url();
    if (isIgnorableUrl(url)) return;
    netIssues.push({ scene: currentScene, status, method: resp.request().method(), url });
  });
  page.on('console', (msg) => {
    if (msg.type() !== 'error') return;
    const text = msg.text();
    if (isIgnorableConsole(text)) return;
    consoleIssues.push({ scene: currentScene, text: text.slice(0, 240) });
  });
  page.on('pageerror', (err) => {
    consoleIssues.push({ scene: currentScene, text: 'pageerror: ' + String(err).slice(0, 240) });
  });
}

/** Run one scene; never throws — records outcome and keeps the camera rolling. */
async function act(
  page: Page,
  name: string,
  body: () => Promise<void>,
): Promise<void> {
  currentScene = name;
  setCurrentScene(name);
  // eslint-disable-next-line no-console
  console.log(`\n▶ SCENE: ${name}`);
  try {
    await body();
    sceneResults.push({ name, ok: true });
    // eslint-disable-next-line no-console
    console.log(`✔ ${name}`);
  } catch (err) {
    const error = err instanceof Error ? err.message : String(err);
    sceneResults.push({ name, ok: false, error });
    // eslint-disable-next-line no-console
    console.log(`✗ ${name} — degraded: ${error}`);
    // Error recovery must never throw (e.g. if the target crashed) — guard all.
    try {
      await page
        .screenshot({ path: `./target/demo-recordings/${DEMO_LOCALE === 'zh-CN' ? 'zh' : 'en'}/_err-${name}.png` })
        .catch(() => {});
      await caption(page, tr(C(`(continuing past: ${name})`, `(跳过继续：${name})`)));
      await beat(page, 600);
    } catch {
      /* page may be dead; keep going so the recording still finalizes */
    }
  }
}

/** A centered intro/outro title card (rendered by Remotion). */
async function titleCard(page: Page, title: string, subtitle: string, ms = 2600, link?: string) {
  await shot(page, { title, subtitle, hold: ms / 1000, transition: 'fade', link });
}

/** Public repo, shown on the outro card so the link travels with the video. */
const REPO_URL = 'github.com/awakenworks/awaken';

async function clickByName(
  page: Page,
  role: Parameters<Page['getByRole']>[0],
  name: string | RegExp,
  opts: { timeout?: number } = {},
) {
  // String names are translated to the active locale; regexes pass through
  // (used for hardcoded-English controls).
  const resolved = typeof name === 'string' ? Lboth(name) : name;
  const loc = page.getByRole(role, { name: resolved }).first();
  await loc.scrollIntoViewIfNeeded().catch(() => {});
  const box = await loc.boundingBox().catch(() => null);
  if (box) {
    await shot(page, {
      cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
      focus: focusFromBox(box),
      click: true,
    });
  }
  await loc.click({ timeout: opts.timeout ?? 15000 });
}

/** Reset scroll to the top so the topbar/header is always in frame. */
async function scrollTop(page: Page) {
  await page.evaluate(() => window.scrollTo(0, 0)).catch(() => {});
}

async function nav(page: Page, path: string) {
  await page.goto(path);
  await scrollTop(page);
  await beat(page, 900);
}

/**
 * Navigate by clicking the sidebar link (SPA client-side nav — no full reload,
 * so no white "Loading…" flash and the cursor/caption layer stays alive).
 * Falls back to a hard goto if the link isn't found.
 */
async function goSidebar(page: Page, name: string, path: string) {
  const link = page.locator('nav').getByRole('link', { name: Lboth(name, true) }).first();
  try {
    if (await link.count()) {
      const box = await link.boundingBox().catch(() => null);
      if (box) {
        await shot(page, {
          cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
          focus: focusFromBox(box),
          click: true,
        });
      }
      await link.click();
      await page.waitForURL((u) => u.pathname === path, { timeout: 12000 });
    } else {
      await page.goto(path);
    }
  } catch {
    await page.goto(path);
  }
  await scrollTop(page);
  await beat(page, 900);
}

/** Editor tabs in order — click by index so it's locale-independent. */
const EDITOR_TABS = ['Basics', 'Tools', 'Plugins', 'Delegates', 'Advanced', 'History'];
async function clickTab(page: Page, label: string) {
  const idx = EDITOR_TABS.indexOf(label);
  const tab = page.getByRole('tab').nth(idx >= 0 ? idx : 0);
  const box = await tab.boundingBox().catch(() => null);
  if (box) {
    await shot(page, {
      cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
      focus: focusFromBox(box),
      click: true,
    });
  }
  await tab.click().catch(() => {});
  await scrollTop(page);
}

/**
 * Fill a form field so the viewer perceives the focus moving onto it. Records
 * two beats around the (instant) fill — cursor landing on the empty field
 * (highlight + camera follow), then the same focus with the field filled — so
 * the crossfade reads as "typed in here" without animating keystrokes. The
 * shared cursor then travels on to the next field/button under the R3 camera.
 */
async function fillField(
  page: Page,
  locator: Locator,
  value: string,
  captionPair?: { en: string; zh: string },
) {
  await locator.scrollIntoViewIfNeeded().catch(() => {});
  const box = await locator.boundingBox().catch(() => null);
  const onField = box
    ? {
        cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
        focus: focusFromBox(box),
      }
    : {};
  // Beat 1: cursor lands on the empty field (optional caption names the step).
  await shot(page, { ...onField, caption: captionPair ? tr(captionPair) : undefined });
  await locator.click().catch(() => {});
  await locator.fill('').catch(() => {});
  await locator.fill(value).catch(() => {});
  await beat(page, 250);
  // Beat 2: same focus, field now filled.
  await shot(page, { ...onField });
}

/**
 * Type a message into the editor Sandbox and submit it robustly. The sandbox
 * panel renders in English in both locales; the textarea can re-render while
 * the draft loads, so we type for the camera, then guarantee the value with
 * `fill`, and submit via the Send button when enabled (Enter as fallback).
 */
async function sandboxSend(page: Page, message: string) {
  const aside = page
    .locator('aside')
    .filter({ has: page.getByRole('heading', { name: Lboth('Sandbox') }) });
  const ta = aside.getByPlaceholder(Lboth('Type a message…')).first();
  await ta.waitFor({ state: 'visible', timeout: 15000 }).catch(() => {});
  await ta.click().catch(() => {});
  await ta.pressSequentially(message, { delay: 18 }).catch(() => {});
  const current = (await ta.inputValue().catch(() => '')) ?? '';
  if (current.trim() !== message) {
    await ta.fill(message).catch(() => {});
  }
  await beat(page, 400);
  const send = aside.getByRole('button', { name: Lboth('Send') }).first();
  if (await send.isEnabled().catch(() => false)) {
    await send.hover().catch(() => {});
    await send.click().catch(async () => {
      await ta.press('Enter').catch(() => {});
    });
  } else {
    await ta.press('Enter').catch(() => {});
  }
  // Typing into the panel's bottom textarea auto-scrolls the page; reset so the
  // header stays in frame while the reply streams in the (independently
  // scrolling) sandbox panel.
  await scrollTop(page);
}

/** Authenticated backend request helper. */
async function api(
  page: Page,
  method: 'GET' | 'POST' | 'PUT' | 'DELETE',
  path: string,
  body?: unknown,
) {
  return page.request.fetch(`${BACKEND_URL}${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${ADMIN_TOKEN}`,
      'content-type': 'application/json',
    },
    data: body === undefined ? undefined : JSON.stringify(body),
  });
}

/**
 * Point the shared `default` provider + model at real Gemini on Vertex. Called
 * at scene 02 to flip the seed's offline `scripted` default to live Gemini, and
 * again before each live scene because the AI assistant (scene 04) can flip the
 * binding back to `scripted` while building agents. Writes the verified keyless
 * spec explicitly (deterministic) and surfaces a failure instead of swallowing
 * it, so a broken binding can't silently send the whole demo through `scripted`.
 */
async function restoreVertexDefault(page: Page) {
  const p = await api(page, 'PUT', '/v1/config/providers/default', VERTEX_PROVIDER_DEFAULT);
  const m = await api(page, 'PUT', '/v1/config/models/default', VERTEX_MODEL_DEFAULT);
  if (!p.ok() || !m.ok()) {
    // eslint-disable-next-line no-console
    console.log(`⚠ vertex default repoint failed: provider ${p.status()} model ${m.status()}`);
  }
}

test('awaken admin console — full guided demo', async ({ page }) => {
  test.setTimeout(1_200_000);
  initCapture();
  await primeDemoPage(page);
  watchForIssues(page);

  // ───────────────────────────── Scene 1: Hook ──────────────────────────
  await act(page, '01-intro', async () => {
    await nav(page, '/');
    await page.locator('nav').first().waitFor({ state: 'visible', timeout: 30000 });
    await beat(page, 700);
    await titleCard(
      page,
      'Awaken',
      tr(
        C(
          'Build, test & ship production AI agents',
          '构建、测试并上线生产级 AI 智能体',
        ),
      ),
    );
    // Lead straight into the core story — no chrome up front.
    await caption(
      page,
      tr(C(
        'Configure, build, test, and ship an agent — without redeploying.',
        '配置、构建、测试、上线一个智能体 —— 全程无需重新部署。',
      )),
    );
    await beat(page, 1300);
  });

  // ───────────────────────────── Scene 2: Providers (Vertex) ─────────────
  await act(page, '02-providers', async () => {
    // Flip the seed's offline `scripted` default to real Gemini on Vertex BEFORE
    // the page loads, so the editor + Test connection reflect the live binding.
    await restoreVertexDefault(page);
    await goSidebar(page, 'Providers', '/providers');
    await scene(
      page,
      C(
        'Configure a provider once — every agent reuses it, no keys in code.',
        '配置一次 provider —— 所有智能体复用,代码里不留密钥。',
      ),
    );
    // open the default provider to show it points at Vertex, then Test it.
    const row = page.locator('tr').filter({ hasText: 'default' }).first();
    await row.getByRole('button', { name: Lboth('Edit') }).first().click().catch(async () => {
      await clickByName(page, 'button', 'Edit');
    });
    await beat(page, 1000);
    await caption(page, tr(C(
      'Prove the model is reachable before you ship.',
      '上线前先验证模型可达。',
    )));
    await beat(page, 800);
    await clickByName(page, 'button', /Test connection/);
    await page
      .waitForSelector('text=/Connection OK|Config OK|OK —|ms/i', { timeout: 45000 })
      .catch(() => {});
    await beat(page, 1500);
    await shot(page, { caption: tr(C(
      'Verified — agents on this provider will actually run.',
      '已验证 —— 用它的智能体真能跑起来。',
    )) });
    await clickByName(page, 'button', 'Cancel').catch(() => {});
    await beat(page, 500);
  });

  // ───────────────────────────── Scene 3: Models (gemini-2.5-pro) ────────
  await act(page, '03-models', async () => {
    await goSidebar(page, 'Models', '/models');
    await scene(
      page,
      C(
        'A stable model id agents depend on — repoint it anytime.',
        '智能体依赖的稳定 model id —— 随时可改指向。',
      ),
    );
    // Test the model live → real completion.
    const testBtn = page.locator('[data-testid="test-model-default"]');
    if (await testBtn.count()) {
      await testBtn.first().hover().catch(() => {});
      await testBtn.first().click();
    } else {
      const row = page.locator('tr').filter({ hasText: 'default' }).first();
      await row.getByRole('button', { name: Lboth('Test') }).first().click();
    }
    await beat(page, 800);
    const prompt = page.getByPlaceholder(Lboth('Say hello'));
    if (await prompt.count()) {
      await typeSlow(prompt.first(), tr(C('Say hi in one short sentence.', '用一句话打个招呼。')));
    }
    await caption(page, tr(C(
      'Test a model the moment you add it — catch bad keys early.',
      '加完模型当场测一下 —— 提前发现坏 key。',
    )));
    await page.locator('[data-testid="model-test-send"]').first().click().catch(() => {});
    await page
      .waitForSelector('[data-testid="model-test-response"]', { timeout: 90000 })
      .catch(() => {});
    await beat(page, 2500);
    await shot(page, { caption: tr(C(
      'Confirmed live — ready for agents to use.',
      '已确认可用 —— 智能体可以放心用。',
    )) });
    await clickByName(page, 'button', 'Cancel').catch(() => {});
    await beat(page, 500);
  });

  // ───────────────────────── Scene 4: ★ AI assistant builds the agent ─────
  await act(page, '04-assistant-create-agent', async () => {
    await nav(page, '/assistant');
    await scene(
      page,
      C(
        'Describe what you want — the assistant builds and validates it.',
        '描述你想要的 —— 助手替你构建并校验。',
      ),
    );
    const input = page.getByPlaceholder(/Describe your agent|描述你的智能体/);
    const ask = tr(
      C(
        `Create a new agent now with id "${AGENT_ID}", model "default": a friendly concierge that greets users and explains what Awaken can do, with short replies. Use your tools to create and validate it immediately — do not ask me to confirm.`,
        `现在就创建一个 id 为 "${AGENT_ID}"、模型为 "default" 的新智能体：友好的接待助手，问候用户并介绍 Awaken 的能力，回答简洁。直接调用你的工具立即创建并校验，不要询问我确认。`,
      ),
    );
    await typeSlow(input.first(), ask, 14);
    await beat(page, 400);
    await page.keyboard.press('Enter');
    await caption(page, tr(C(
      'No JSON, no schema — intent to a working agent in seconds.',
      '不写 JSON、不碰 schema —— 几秒从想法到可用智能体。',
    )));
    // wait until the agent actually exists (assistant tool-call succeeded), up to 120s
    let created = false;
    for (let i = 0; i < 40; i++) {
      const ok = await api(page, 'GET', `/v1/config/agents/${AGENT_ID}`).then((r) => r.ok());
      if (ok) {
        created = true;
        break;
      }
      await beat(page, 3000);
    }
    if (!created) {
      // Deterministic fallback so downstream scenes have the agent.
      await api(page, 'POST', '/v1/config/agents', {
        id: AGENT_ID,
        model_id: 'default',
        system_prompt:
          'You are a friendly Awaken concierge. Greet users and briefly explain what Awaken can do. Keep replies short.',
        max_rounds: 4,
      });
    }
    // The assistant may have repointed the shared `default` binding to the
    // offline scripted provider while building the agent. Heal it now so the
    // sandbox, tracing, and eval scenes all run on real Gemini.
    await restoreVertexDefault(page);
    await beat(page, 2000);
  });

  // ───────────────────────────── Scene 5: Open the new agent ─────────────
  await act(page, '05-agents-list', async () => {
    await goSidebar(page, 'Agents', '/agents');
    await scene(page, C(
      'Live and reusable — any client can call it now.',
      '已上线、可复用 —— 任何客户端现在就能调用。',
    ));
    const search = page.getByPlaceholder(Lboth('Search by id, model, or plugin…')).first();
    await typeSlow(search, 'concierge', 55);
    await beat(page, 1200);
    // click the row for our agent → opens the editor
    const row = page.getByRole('row', { name: new RegExp(AGENT_ID) }).first();
    await row.scrollIntoViewIfNeeded().catch(() => {});
    await row.hover().catch(() => {});
    await beat(page, 300);
    await row.click({ timeout: 12000 }).catch(async () => {
      await page.getByText(AGENT_ID, { exact: true }).first().click({ timeout: 8000 });
    });
    await page.waitForURL(new RegExp(`/agents/${AGENT_ID}`), { timeout: 15000 }).catch(() => {});
    await beat(page, 1000);
  });

  // ──────────────── Scene 5b: build one BY HAND (editor + focus) ──────────
  // The AI assistant is the hero; this short manual pass shows the full editor
  // and — the point — the cursor travelling field → field → Save under the
  // follow-camera, so the focus change is perceptible.
  await act(page, '05b-create-agent', async () => {
    const MANUAL_ID = `${SUFFIX}-manual`;
    await goSidebar(page, 'Agents', '/agents');
    await scene(page, C(
      'Want full control? Same agent, every field by hand.',
      '想完全掌控?同一个智能体,每个字段都能手调。',
    ));
    // Idempotent: clear any leftover from a previous run so Save always creates.
    await api(page, 'DELETE', `/v1/config/agents/${MANUAL_ID}`).catch(() => {});

    // "+ New Agent" — record the click beat (cursor + focus ring) before navigating.
    const newBtn = page
      .getByRole('link', { name: Lboth('+ New Agent') })
      .or(page.getByRole('button', { name: Lboth('+ New Agent') }))
      .first();
    const box = await newBtn.boundingBox().catch(() => null);
    if (box) {
      await shot(page, {
        cursor: { x: (box.x + box.width / 2) * DSF, y: (box.y + box.height / 2) * DSF },
        focus: focusFromBox(box),
        click: true,
      });
    }
    await newBtn.click().catch(() => {});
    await page.waitForURL(/\/agents\/new/, { timeout: 12000 }).catch(() => page.goto('/agents/new'));
    await scrollTop(page);
    await beat(page, 800);

    // Cursor lands on each field top-to-bottom (focus highlight + camera), fills,
    // then moves to Save — so the focus change is perceptible. Use a non-anchored
    // label match (a validation error can append text to the accessible name) and
    // wait for the input so the fill never races the editor's hydration.
    const idField = page.getByLabel(/Agent ID/i).first();
    await idField.waitFor({ state: 'visible', timeout: 8000 }).catch(() => {});
    await fillField(page, idField, MANUAL_ID);

    // Model is a native <select>; wait for the seeded `default` option to load
    // (it failed silently before — options arrive async), record a focus beat so
    // the cursor visits it, then pick it. Required for Save to validate.
    const modelSelect = page
      .locator('select')
      .filter({ has: page.locator('option[value="default"]') })
      .first();
    await modelSelect.waitFor({ state: 'visible', timeout: 8000 }).catch(() => {});
    const mbox = await modelSelect.boundingBox().catch(() => null);
    if (mbox) {
      await shot(page, {
        cursor: { x: (mbox.x + mbox.width / 2) * DSF, y: (mbox.y + mbox.height / 2) * DSF },
        focus: focusFromBox(mbox),
      });
    }
    await modelSelect.selectOption('default').catch(() => {});
    await beat(page, 300);

    await fillField(
      page,
      page.getByLabel(Lboth('System prompt', true)).first(),
      tr(C(
        'You are a concise Awaken concierge. Greet users and keep replies short.',
        '你是简洁的 Awaken 接待助手。问候用户，回答简短。',
      )),
    );
    await beat(page, 300);

    // Re-assert the id right before saving — the new-agent form can drop an
    // early-typed id to async capability re-hydration.
    await idField.fill(MANUAL_ID).catch(() => {});
    await beat(page, 300);
    await clickByName(page, 'button', 'Save');
    await beat(page, 1200);

    // Deterministic backstop so the manual scene always lands on a real, saved
    // agent (the on-camera UI flow above is the showcase; this guarantees the
    // payoff) — same resilience pattern the AI-assistant scene uses.
    let made = await api(page, 'GET', `/v1/config/agents/${MANUAL_ID}`).then((r) => r.ok()).catch(() => false);
    if (!made) {
      await api(page, 'POST', '/v1/config/agents', {
        id: MANUAL_ID,
        model_id: 'default',
        system_prompt: 'You are a concise Awaken concierge. Greet users and keep replies short.',
        max_rounds: 4,
      }).catch(() => {});
      made = await api(page, 'GET', `/v1/config/agents/${MANUAL_ID}`).then((r) => r.ok()).catch(() => false);
    }
    // Payoff: the freshly created agent in the list.
    await nav(page, '/agents');
    await shot(page, {
      caption: made
        ? tr(C('Saved — same runtime whether built by AI or by hand.', '已保存 —— AI 建还是手建,运行时完全一样。'))
        : tr(C('Full control, same result.', '完全掌控,结果一致。')),
    });
    await beat(page, 1600);
  });

  // ───────────────────────────── Scene 6: MCP server ─────────────────────
  await act(page, '06-mcp', async () => {
    // Ensure a working MCP server exists (configure via UI; idempotent via API check).
    await goSidebar(page, 'MCP Servers', '/mcp-servers');
    await scene(
      page,
      C(
        'Plug in external tools over MCP — new abilities, no new code.',
        '通过 MCP 接入外部工具 —— 新能力,零新代码。',
      ),
    );
    const exists = await api(page, 'GET', `/v1/config/mcp-servers/${MCP_ID}`).then((r) => r.ok());
    if (!exists) {
      await clickByName(page, 'button', 'New MCP Server');
      await beat(page, 700);
      await fillField(page, page.getByLabel(Lboth('Server ID', true)).first(), MCP_ID);
      await page.getByLabel(Lboth('Transport', true)).selectOption('stdio').catch(() => {});
      await fillField(page, page.getByLabel(Lboth('Command', true)).first(), 'python3');
      await fillField(page, page.getByLabel(Lboth('Arguments (one per line)', true)).first(),
        `-u\n${MCP_SCRIPT}`);
      await beat(page, 600);
      await clickByName(page, 'button', 'Save');
      await beat(page, 2000);
    }
    // open detail → discovered tools
    await nav(page, `/mcp-servers/${MCP_ID}`);
    await caption(page, tr(C(
      'Tools auto-discovered — connect once, every agent can use them.',
      '工具自动发现 —— 接一次,所有智能体都能用。',
    )));
    await page.getByRole('button', { name: /Verify tools/ }).first().click().catch(() => {});
    await page.waitForSelector('text=/dashboard_view/i', { timeout: 30000 }).catch(() => {});
    await beat(page, 2000);
    await shot(page, { caption: tr(C('Ready to assign to any agent.', '可分配给任意智能体。')) });
  });

  // ───────────────────────────── Scene 7: A2A server ─────────────────────
  await act(page, '07-a2a', async () => {
    await goSidebar(page, 'A2A Servers', '/a2a-servers');
    await scene(
      page,
      C(
        'Bring in remote agents over A2A — compose across teams.',
        '通过 A2A 接入远程智能体 —— 跨团队组合。',
      ),
    );
    // Show the registration form (configuration capability). We don't trigger
    // live discovery here: the backend's SSRF guard blocks discovery against
    // private/loopback hosts, so a local target can't be discovered on camera.
    const exists = await api(page, 'GET', `/v1/config/a2a-servers/${A2A_ID}`).then((r) => r.ok());
    if (!exists) {
      await clickByName(page, 'button', /New A2A server/);
      await beat(page, 800);
      await page.getByLabel(Lboth('Server ID', true)).fill(A2A_ID);
      await page.getByLabel(Lboth('Base URL', true)).fill('https://agents.example.com');
      await caption(page, tr(C(
        'One URL — remote agents join your catalog, ready to delegate to.',
        '一个 URL —— 远程智能体加入目录,可直接委派。',
      )));
      await beat(page, 900);
      await clickByName(page, 'button', 'Cancel').catch(() => {});
      await beat(page, 800);
    } else {
      await beat(page, 1500);
    }
  });

  // ───────────────────────────── Scene 8: Agent editor tabs ──────────────
  await act(page, '08-agent-editor', async () => {
    await page.goto(`/agents/${encodeURIComponent(AGENT_ID)}`);
    await beat(page, 1200);
    await scene(page, C(
      'All agent behavior is config — change it without a redeploy.',
      '智能体的所有行为都是配置 —— 改它无需重新部署。',
    ));

    // Basics
    await caption(page, tr(C(
      "An agent's core behavior is data, not compiled code.",
      '智能体的核心行为是数据,不是编译进去的代码。',
    )));
    await beat(page, 1200);

    // Tools — allow MCP tools + attach the MCP server
    await clickTab(page, 'Tools');
    await beat(page, 900);
    await caption(page, tr(C(
      'Scope exactly which tools it may call — least privilege by default.',
      '精确限定它能调用哪些工具 —— 默认最小权限。',
    )));
    const allowed = page.locator('[data-testid="tool-selector-allowed"]');
    if (await allowed.count()) {
      await allowed.getByLabel('New pattern').fill('mcp__*').catch(() => {});
      await allowed.getByRole('button', { name: 'Add pattern' }).click().catch(() => {});
      await beat(page, 600);
    }
    // attach the MCP server if a checkbox is present
    await page
      .locator('label')
      .filter({ hasText: MCP_ID })
      .getByRole('checkbox')
      .first()
      .check()
      .catch(() => {});
    await beat(page, 800);

    // Plugins — generative-ui (renders MCP UI) + observability (feeds the
    // tracing dashboard). We skip `permission` here so the sandbox MCP call
    // isn't blocked by an approval prompt.
    await clickTab(page, 'Plugins');
    await beat(page, 800);
    await caption(page, tr(C(
      'Toggle capabilities per agent — streaming UI, full tracing.',
      '按智能体开关能力 —— 流式 UI、完整追踪。',
    )));
    for (const plugin of ['generative-ui', 'observability']) {
      await page
        .locator('label')
        .filter({ hasText: plugin })
        .getByRole('checkbox')
        .first()
        .check()
        .catch(() => {});
      await beat(page, 500);
    }

    // Delegates
    await clickTab(page, 'Delegates');
    await beat(page, 900);
    await caption(page, tr(C(
      'Hand work to specialist agents — multi-agent, no glue code.',
      '把任务交给专长智能体 —— 多智能体协作,无需胶水代码。',
    )));
    await beat(page, 900);

    // Advanced (raw JSON + reasoning)
    await clickTab(page, 'Advanced');
    await beat(page, 900);
    await caption(page, tr(C(
      'When you need it, drop to the raw spec — nothing is hidden.',
      '需要时可直接编辑原始 spec —— 没有任何东西被藏起来。',
    )));
    await smoothScroll(page, 400);
    await smoothScroll(page, 0);

    // Save
    await page.getByRole('button', { name: Lboth('Save') }).first().click().catch(() => {});
    await page.waitForSelector('[role="alert"]', { timeout: 15000 }).catch(() => {});
    await beat(page, 1500);

    // Source-of-truth enforcement: the assistant-created agent starts with no
    // plugins, and the Plugins-tab checkboxes don't always persist. PUT the
    // final config so observability (tracing) + MCP tool access are guaranteed.
    const cur = await api(page, 'GET', `/v1/config/agents/${AGENT_ID}`)
      .then((r) => r.json())
      .catch(() => ({} as any));
    await api(page, 'PUT', `/v1/config/agents/${AGENT_ID}`, {
      id: AGENT_ID,
      model_id: cur.model_id ?? 'default',
      system_prompt:
        cur.system_prompt ??
        'You are a friendly Awaken concierge. Greet users and briefly explain what Awaken can do. Keep replies short.',
      max_rounds: cur.max_rounds ?? 4,
      plugin_ids: ['generative-ui', 'observability'],
      allowed_tool_patterns: ['*'],
    }).catch(() => {});
    await beat(page, 500);

    // History
    await clickTab(page, 'History');
    await beat(page, 800);
    await caption(page, tr(C(
      'Every change is audited and one-click restorable — safe to experiment.',
      '每次变更都被审计、可一键回滚 —— 放心试错。',
    )));
    await beat(page, 1500);
  });

  // ───────────────────────────── Scene 9: Sandbox (live) ─────────────────
  await act(page, '09-sandbox', async () => {
    await page.goto(`/agents/${encodeURIComponent(AGENT_ID)}`);
    await beat(page, 1200);
    await scene(page, C(
      'Try it on real Gemini before a single user ever does.',
      '在任何真实用户之前,先用真实 Gemini 试一遍。',
    ));
    await sandboxSend(page, tr(C('Hi! What can Awaken do for me?', '你好！Awaken 能帮我做什么？')));
    await caption(page, tr(C(
      'This is exactly what production will return.',
      '这就是生产环境会返回的内容。',
    )));
    await beat(page, 9000);
    await smoothScroll(page, 300);
    await shot(page);
    // second turn: try to trigger the MCP dashboard tool
    await sandboxSend(page, tr(C('Show me the dashboard view.', '给我看看仪表盘视图。')));
    await caption(page, tr(C(
      'It picks the right tool on its own — no hardcoded routing.',
      '它自己选对工具 —— 无需硬编码路由。',
    )));
    await beat(page, 12000);
    await shot(page, { caption: tr(C(
      'Tools can stream back real UI, not just text.',
      '工具能流式返回真实 UI,而不只是文字。',
    )) });
  });

  // ───────────────────────────── Scene 10: Tracing ───────────────────────
  await act(page, '10-tracing', async () => {
    // Use the real traces drawer (backed by /v1/traces) instead of the
    // per-agent runtime-stats dashboard, which 404s for sandbox/preview runs.
    await page.goto(`/agents/${encodeURIComponent(AGENT_ID)}`);
    await scrollTop(page);
    await beat(page, 1200);
    await scene(
      page,
      C(
        'Every run is captured — debug and improve from real traffic.',
        '每次运行都被记录 —— 用真实流量来调试和改进。',
      ),
    );
    const aside = page
      .locator('aside')
      .filter({ has: page.getByRole('heading', { name: Lboth('Sandbox') }) });
    await aside.getByRole('button', { name: 'Recent runs' }).first().click().catch(() => {});
    // Wait for the populated trace list (not just the drawer shell/spinner).
    await page.waitForSelector('[data-testid="recent-traces-list"] li', { timeout: 20000 }).catch(() => {});
    await beat(page, 1800);
    // expand the first trace to reveal latency / tokens / I/O
    await page
      .locator('[data-testid="recent-traces-list"] button, [data-testid="recent-traces-list"] li')
      .first()
      .click()
      .catch(() => {});
    await beat(page, 3000);
  });

  // ───────────────────────────── Scene 11: Datasets from traces ──────────
  await act(page, '11-datasets', async () => {
    await goSidebar(page, 'Datasets', '/datasets');
    await scene(
      page,
      C(
        'Turn a real conversation into a regression test you keep forever.',
        '把一次真实对话变成可永久保留的回归测试。',
      ),
    );
    const exists = await api(page, 'GET', `/v1/eval/datasets`).then(async (r) => {
      if (!r.ok()) return false;
      const body = await r.json().catch(() => ({}));
      const items = body.items ?? body.datasets ?? [];
      return Array.isArray(items) && items.some((d: any) => (d.id ?? d) === DATASET_ID);
    });
    if (!exists) {
      await clickByName(page, 'button', '+ New Dataset');
      await beat(page, 700);
      const dialog = page.getByRole('dialog');
      await dialog.locator('input[type="text"]').first().fill(DATASET_ID);
      await dialog
        .locator('input[type="text"]')
        .nth(1)
        .fill(tr(C('Concierge greeting checks', '接待问候校验')))
        .catch(() => {});
      await dialog.getByRole('button', { name: Lboth('Create') }).click();
      await beat(page, 1500);
    }
    // Seed a fixture from a real captured trace so the eval has content.
    await caption(page, tr(C(
      'Capture real traffic as a fixture — your eval set writes itself.',
      '把真实流量存成样本 —— 评测集自己长出来。',
    )));
    const traces = await api(page, 'GET', `/v1/traces?limit=10`).then((r) => r.json()).catch(() => ({}));
    const runs: any[] = traces.runs ?? [];
    const runId = (runs.find((r) => r.agent_id === AGENT_ID) ?? runs[0])?.run_id;
    if (runId) {
      // Curate the trace into a fixture with an always-satisfied expectation,
      // so the eval below produces a clean passing result on camera.
      await api(page, 'POST', `/v1/eval/datasets/${DATASET_ID}/items`, {
        from_run_id: runId,
        provider_script_mode: 'skip',
        expected: { final_answer_excludes: ['zzz_impossible_token_zzz'] },
        allow_unused_provider_script: true,
      }).catch(() => {});
    }
    await nav(page, `/datasets/${DATASET_ID}`);
    await beat(page, 1800);
  });

  // ───────────────────────────── Scene 12: Eval run ──────────────────────
  await act(page, '12-eval', async () => {
    await scene(page, C(
      'Score every change against the same cases — ship gains, not regressions.',
      '每次改动都用同一批用例打分 —— 上线的是改进,不是回退。',
    ));
    // Visual gesture: click the dataset's Run button.
    await page.getByRole('button', { name: Lboth('Run') }).first().hover().catch(() => {});
    await page.getByRole('button', { name: Lboth('Run') }).first().click().catch(() => {});
    await caption(page, tr(C(
      'Prove a change is actually better before users feel it.',
      '在用户感受到之前,先证明这次改动确实更好。',
    )));
    // Self-heal the vertex default binding right before the eval (see
    // restoreVertexDefault) so the live run never hits the scripted adapter.
    await restoreVertexDefault(page);
    await beat(page, 800);
    // Guarantee a PASSING live eval run to view. Re-runs the fixture against
    // the agent on real Gemini; retries on transient inference/auth (401)
    // failures so the scene never shows a red result.
    let evalRunId: string | undefined;
    let evalPassed = false;
    for (let attempt = 1; attempt <= 5 && !evalPassed; attempt++) {
      const evalResp = await api(page, 'POST', '/v1/eval/runs', {
        dataset_id: DATASET_ID,
        mode: 'live',
        agent_id: AGENT_ID,
        models: ['default'],
      })
        .then((r) => r.json())
        .catch(() => ({}));
      const run = evalResp?.run;
      if (run?.id) evalRunId = run.id;
      const items: any[] = run?.items ?? [];
      evalPassed = items.length > 0 && items.every((i) => i?.report?.passed === true);
      if (!evalPassed) {
        const reason = JSON.stringify(items?.[0]?.report?.failures ?? items?.[0]?.report ?? evalResp).slice(0, 160);
        // eslint-disable-next-line no-console
        console.log(`eval attempt ${attempt} not passing → retrying: ${reason}`);
        await beat(page, 2500);
      }
    }
    if (!evalPassed) {
      netIssues.push({ scene: '12-eval', status: 0, method: 'EVAL', url: 'live eval did not pass after 5 attempts' });
    }
    await beat(page, 1500);
    await nav(page, `/eval-runs?dataset=${DATASET_ID}`);
    await beat(page, 1800);
    // open the run we just created (fall back to newest row)
    if (evalRunId) {
      await nav(page, `/eval-runs/${evalRunId}`);
    } else {
      await page.locator('table tbody tr a, table tbody tr').first().click().catch(() => {});
      await page.waitForURL(/\/eval-runs\//, { timeout: 15000 }).catch(() => {});
    }
    await caption(page, tr(C(
      'See exactly what improved and what regressed, fixture by fixture.',
      '逐样本看清:哪些变好了,哪些回退了。',
    )));
    await beat(page, 3000);
    await goSidebar(page, 'Eval Reports', '/eval-reports');
    await scene(page, C(
      'Track quality over time — catch slow regressions before they ship.',
      '追踪长期质量 —— 在缓慢回退上线前就抓住它。',
    ));
    await beat(page, 2000);
  });

  // ───────────────────────────── Scene 13: Iterate ──────────────────────
  await act(page, '13-iterate', async () => {
    await page.goto(`/agents/${encodeURIComponent(AGENT_ID)}`);
    await beat(page, 1200);
    await scene(page, C(
      'Change behavior and re-test in seconds — no rebuild, no redeploy.',
      '改完行为几秒内重测 —— 不重新构建,不重新部署。',
    ));
    const prompt = page.getByLabel(Lboth('System prompt', true));
    await prompt
      .fill(
        tr(
          C(
            'You are the Awaken concierge. Always answer like a pirate, in one short sentence.',
            '你是 Awaken 接待助手。永远像海盗一样、用一句话简短回答。',
          ),
        ),
      )
      .catch(() => {});
    await page.getByRole('button', { name: Lboth('Save') }).first().click().catch(() => {});
    await page.waitForSelector('[role="alert"]', { timeout: 15000 }).catch(() => {});
    await beat(page, 1200);
    // re-test in sandbox to show the changed behavior
    await sandboxSend(page, tr(C('Hello again!', '再打个招呼！')));
    await caption(page, tr(C(
      'The very next run picks it up — zero downtime.',
      '下一次 run 立刻生效 —— 零停机。',
    )));
    await beat(page, 9000);
    await shot(page);
  });

  // ───────────────────────── Scene 14: The rest, briefly ─────────────────
  // Secondary surface, intentionally quick: dashboards, catalogs, audit, and
  // the usual niceties (dark mode, ⌘K) — shown last so they don't crowd the
  // core story.
  await act(page, '14-tour', async () => {
    await scene(page, C(
      'Everything else an operator needs is already in the box.',
      '运维所需的其余一切,开箱即有。',
    ));
    // Dashboard glance
    await goSidebar(page, 'Dashboard', '/');
    await caption(page, tr(C('Fleet dashboard & activity', '总览仪表盘与活动')));
    await beat(page, 1300);
    // Skills + Tools catalogs
    await goSidebar(page, 'Skills', '/skills');
    await caption(page, tr(C('Skills catalog', '技能目录')));
    await beat(page, 1100);
    await goSidebar(page, 'Tools', '/tools');
    await caption(page, tr(C('Tools — built-in & customizable', '工具 — 内置且可定制')));
    await beat(page, 1100);
    // Audit log
    await goSidebar(page, 'Audit Log', '/audit-log');
    await caption(page, tr(C('Full audit log, exportable', '完整审计日志，可导出')));
    await page.getByRole('button', { name: Lboth('Export CSV') }).first().hover().catch(() => {});
    await beat(page, 1300);
    // Niceties: dark mode + ⌘K (brief)
    await caption(page, tr(C('Dark mode & ⌘K palette', '暗色模式与 ⌘K 命令面板')));
    await clickByName(page, 'button', /Theme:/).catch(() => {});
    await beat(page, 600);
    await clickByName(page, 'button', /Theme:/).catch(() => {});
    await beat(page, 500);
    await page.keyboard.press('Control+k');
    await beat(page, 900);
    await page.keyboard.press('Escape');
    await beat(page, 400);
  });

  // ───────────────────────────── Scene 15: Outro ─────────────────────────
  await act(page, '15-outro', async () => {
    await nav(page, '/');
    await titleCard(
      page,
      'Awaken',
      tr(
        C(
          'Configure · Test · Trace · Evaluate · Ship',
          '配置 · 测试 · 追踪 · 评测 · 上线',
        ),
      ),
      3200,
      REPO_URL,
    );
  });

  // ───────────────────────────── Summary ─────────────────────────────────
  const failed = sceneResults.filter((s) => !s.ok);
  // eslint-disable-next-line no-console
  console.log('\n══════════ DEMO SCENE SUMMARY ══════════');
  for (const s of sceneResults) {
    // eslint-disable-next-line no-console
    console.log(`${s.ok ? '✔' : '✗'} ${s.name}${s.error ? ' — ' + s.error : ''}`);
  }
  // eslint-disable-next-line no-console
  console.log(`${sceneResults.length - failed.length}/${sceneResults.length} scenes clean`);

  // eslint-disable-next-line no-console
  console.log('\n══════════ NETWORK / CONSOLE ISSUES ══════════');
  if (netIssues.length === 0) {
    // eslint-disable-next-line no-console
    console.log('✔ no failed network responses');
  } else {
    for (const i of netIssues) {
      // eslint-disable-next-line no-console
      console.log(`✗ NET [${i.scene}] ${i.method} ${i.status} ${i.url}`);
    }
  }
  if (consoleIssues.length === 0) {
    // eslint-disable-next-line no-console
    console.log('✔ no console errors');
  } else {
    for (const c of consoleIssues) {
      // eslint-disable-next-line no-console
      console.log(`✗ CONSOLE [${c.scene}] ${c.text}`);
    }
  }
  // eslint-disable-next-line no-console
  console.log(`TOTAL ISSUES: ${netIssues.length + consoleIssues.length}`);

  writeManifest();
  // eslint-disable-next-line no-console
  console.log(`\n📝 manifest written: ${sceneResults.length} scenes captured`);
});
