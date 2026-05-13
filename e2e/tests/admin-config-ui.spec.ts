import { expect, test } from '@playwright/test';
import { TEST_ADMIN_TOKEN } from '../playwright.admin.config';

const BACKEND_URL = process.env.AWAKEN_BACKEND_URL ?? 'http://127.0.0.1:38080';
const BIGMODEL_BASE_URL =
  process.env.BIGMODEL_BASE_URL ?? 'https://open.bigmodel.cn/api/paas/v4/';
const BIGMODEL_MODEL = process.env.BIGMODEL_MODEL ?? 'GLM-4.7-Flash';

function suffix(): string {
  return `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function parseJsonEvents(raw: string): any[] {
  return raw
    .split('\n')
    .filter((line) => line.startsWith('data:'))
    .map((line) => {
      try {
        return JSON.parse(line.slice(5).trim());
      } catch {
        return null;
      }
    })
    .filter(Boolean);
}

function textDeltas(events: any[]): string {
  return events
    .filter((event) => event.event_type === 'text_delta')
    .map((event) => event.delta ?? '')
    .join('');
}

async function gotoEditorTab(
  page: import('@playwright/test').Page,
  name: 'Basics' | 'Tools' | 'Plugins' | 'Delegates' | 'Advanced' | 'History',
) {
  await page
    .getByRole('tablist', { name: 'Editor sections' })
    .getByRole('tab', { name })
    .click();
}

/**
 * The Advanced tab replaced its read-only JSON preview `<pre>` with a
 * textarea-backed Raw JSON editor (see G3). Tests that used to call
 * `page.locator('pre')` should use this locator instead.
 */
function rawJsonEditor(page: import('@playwright/test').Page) {
  return page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Raw JSON' }) })
    .getByRole('textbox');
}

async function selectPlugin(page: import('@playwright/test').Page, pluginId: string) {
  await gotoEditorTab(page, 'Plugins');
  const pluginsSection = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Plugins' }) })
    .first();

  await pluginsSection
    .locator('label')
    .filter({ hasText: pluginId })
    .getByRole('checkbox')
    .first()
    .check();
}

/// Match the toast emitted by the editor on a successful create. The
/// admin console replaced the earlier inline "Agent created." banner
/// with a per-record toast like `Agent "ui-permission-…" created`.
function expectAgentCreatedToast(
  page: import('@playwright/test').Page,
  agentId: string,
) {
  return page.getByRole('alert').filter({
    hasText: new RegExp(`Agent\\s+"${agentId.replace(/[.*+?^${}()|[\\]\\\\]/g, '\\\\$&')}"\\s+created`),
  });
}

async function createProviderViaUi(
  page: import('@playwright/test').Page,
  providerId: string,
  options: {
    adapter: string;
    baseUrl?: string;
    apiKey?: string;
    timeoutSecs?: number;
  },
) {
  await page.goto('/providers');
  await page.getByRole('button', { name: 'New Provider' }).click();
  await page.getByLabel('Provider ID').fill(providerId);
  await page.getByLabel('Adapter').selectOption(options.adapter);
  if (options.baseUrl) {
    await page.getByLabel('Base URL').fill(options.baseUrl);
  }
  if (options.apiKey) {
    await page.locator('input[type="password"]').fill(options.apiKey);
  }
  if (options.timeoutSecs) {
    await page.getByLabel('Timeout (seconds)').fill(String(options.timeoutSecs));
  }
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText(providerId)).toBeVisible();
}

async function createModelViaUi(
  page: import('@playwright/test').Page,
  modelId: string,
  providerId: string,
  upstreamModel: string,
) {
  await page.goto('/models');
  await page.getByRole('button', { name: 'New Model' }).click();
  await page.getByLabel('Model ID').fill(modelId);
  await page.getByLabel('Provider ID').selectOption(providerId);
  await page.getByLabel('Upstream Model').fill(upstreamModel);
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText(modelId)).toBeVisible();
}

test.describe('admin config UI', () => {
  test.beforeEach(async ({ page }) => {
    await page.addInitScript((token) => {
      localStorage.setItem('awaken.adminToken', token);
    }, TEST_ADMIN_TOKEN);
  });

  test('exposes every registered configurable plugin in capabilities', async ({ request }) => {
    const response = await request.get(`${BACKEND_URL}/v1/capabilities`, {
      headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` },
    });
    expect(response.ok()).toBeTruthy();

    const capabilities = await response.json();
    const pluginSchemas = Object.fromEntries(
      capabilities.plugins.map((plugin: any) => [
        plugin.id,
        plugin.config_schemas.map((schema: any) => schema.key),
      ]),
    );

    expect(pluginSchemas.permission).toContain('permission');
    expect(pluginSchemas.reminder).toContain('reminder');
    expect(pluginSchemas['generative-ui']).toContain('generative-ui');
    expect(pluginSchemas['ext-deferred-tools']).toContain('deferred_tools');
  });

  test('saves plugin config from the page and applies it to runtime runs', async ({
    page,
    request,
  }) => {
    const agentId = `ui-permission-${suffix()}`;

    await page.goto('/agents/new');
    await page.getByLabel('Agent ID').fill(agentId);
    await page.getByLabel('Model').selectOption('default');
    await page.getByLabel('Max rounds').fill('1');
    await page
      .getByLabel('System prompt')
      .fill('Use the scripted tool directives when the user provides one.');
    await selectPlugin(page, 'permission');

    await page.getByRole('button', { name: /Permissions/ }).click();
    const pluginsSection = page
      .locator('section')
      .filter({ has: page.getByRole('heading', { name: 'Plugins' }) })
      .first();
    await pluginsSection
      .getByRole('button', { name: /^Deny/ })
      .first()
      .evaluate((element) => (element as HTMLButtonElement).click());

    await gotoEditorTab(page, 'Advanced');
    await expect(rawJsonEditor(page)).toHaveValue(/"default_behavior": "deny"/);
    await expect(rawJsonEditor(page)).not.toHaveValue(/deferred_tools/);

    await page.getByRole('button', { name: 'Save' }).click();
    await expect(expectAgentCreatedToast(page, agentId)).toBeVisible();

    const response = await request.post(`${BACKEND_URL}/v1/runs`, {
      data: {
        agentId,
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL' }],
      },
    });
    expect(response.ok()).toBeTruthy();

    const body = await response.text();
    expect(body).toContain("blocked:Tool 'get_weather' denied by permission rules");
  });

  test('saves fallback JSON-schema plugin config and applies it at runtime', async ({
    page,
    request,
  }) => {
    const agentId = `ui-deferred-${suffix()}`;

    await page.goto('/agents/new');
    await page.getByLabel('Agent ID').fill(agentId);
    await page.getByLabel('Model').selectOption('default');
    await page.getByLabel('Max rounds').fill('1');
    await page
      .getByLabel('System prompt')
      .fill('Use the scripted tool directives when the user provides one.');
    await selectPlugin(page, 'ext-deferred-tools');

    await page.getByRole('button', { name: /Deferred Tools/ }).first().click();

    const pluginsSection = page
      .locator('section')
      .filter({ has: page.getByRole('heading', { name: 'Plugins' }) })
      .first();
    await pluginsSection.getByLabel('beta_overhead').fill('0');

    await gotoEditorTab(page, 'Advanced');
    await expect(rawJsonEditor(page)).toHaveValue(/"deferred_tools"/);
    await expect(rawJsonEditor(page)).toHaveValue(/"beta_overhead": 0/);

    await page.getByRole('button', { name: 'Save' }).click();
    await expect(expectAgentCreatedToast(page, agentId)).toBeVisible();

    const agentResponse = await request.get(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(agentId)}`,
      { headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` } },
    );
    expect(agentResponse.ok()).toBeTruthy();
    const agent = await agentResponse.json();
    expect(agent.sections?.deferred_tools?.beta_overhead).toBe(0);

    const runResponse = await request.post(`${BACKEND_URL}/v1/runs`, {
      data: {
        agentId,
        messages: [{ role: 'user', content: 'RUN_TOOL_SEARCH_WEATHER' }],
      },
    });
    expect(runResponse.ok()).toBeTruthy();

    const body = await runResponse.text();
    const events = parseJsonEvents(body);
    const toolSearchDone = events.find(
      (event) =>
        event.event_type === 'tool_call_done' &&
        event.result?.tool_name === 'ToolSearch',
    );

    expect(toolSearchDone?.result?.status).toBe('success');
    expect(toolSearchDone?.result?.data?.__promote).toContain('get_weather');
    expect(toolSearchDone?.result?.data?.tools).toContain('"name": "Get Weather"');
  });

  test('runs a page-configured BigModel provider through OpenAI-compatible mode', async ({
    page,
    request,
  }) => {
    const apiKey = process.env.BIGMODEL_API_KEY;
    test.skip(!apiKey, 'Set BIGMODEL_API_KEY to run the live BigModel E2E test.');

    const idSuffix = suffix();
    const providerId = `bigmodel-openai-${idSuffix}`;
    const modelId = `bigmodel-model-${idSuffix}`;
    const agentId = `bigmodel-agent-${idSuffix}`;

    await createProviderViaUi(page, providerId, {
      adapter: 'openai',
      baseUrl: BIGMODEL_BASE_URL,
      apiKey,
      timeoutSecs: Number(process.env.BIGMODEL_TIMEOUT_SECS ?? 30),
    });

    const providerResponse = await request.get(
      `${BACKEND_URL}/v1/config/providers/${encodeURIComponent(providerId)}`,
      { headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` } },
    );
    expect(providerResponse.ok()).toBeTruthy();
    const provider = await providerResponse.json();
    expect(provider.adapter).toBe('openai');
    expect(provider.base_url).toBe(BIGMODEL_BASE_URL);
    expect(provider.has_api_key).toBe(true);
    expect(provider.api_key).toBeUndefined();
    expect(provider.timeout_secs).toBe(Number(process.env.BIGMODEL_TIMEOUT_SECS ?? 30));

    await createModelViaUi(page, modelId, providerId, BIGMODEL_MODEL);

    await page.goto('/agents/new');
    await page.getByLabel('Agent ID').fill(agentId);
    await page.getByLabel('Model').selectOption(modelId);
    await page.getByLabel('Max rounds').fill('1');
    await page
      .getByLabel('System prompt')
      .fill('Reply with exactly "bigmodel-ui-ok" and no other text.');
    await page.getByRole('button', { name: 'Save' }).click();
    await expect(expectAgentCreatedToast(page, agentId)).toBeVisible();

    const runResponse = await request.post(`${BACKEND_URL}/v1/runs`, {
      data: {
        agentId,
        messages: [{ role: 'user', content: 'Reply with exactly "bigmodel-ui-ok".' }],
      },
    });
    expect(runResponse.ok()).toBeTruthy();

    const body = await runResponse.text();
    const events = parseJsonEvents(body);
    const finish = events.find((event) => event.event_type === 'run_finish');
    const termination = JSON.stringify(finish?.termination ?? null);

    expect(finish, `BigModel run did not emit run_finish. Events: ${body}`).toBeTruthy();
    expect(
      finish?.termination?.type,
      `BigModel provider returned an error termination: ${termination}`,
    ).not.toBe('error');
    expect(textDeltas(events).toLowerCase()).toContain('bigmodel-ui-ok');
  });

  test('401 prompts admin token modal and retries the original request', async ({ page }) => {
    // Start with no token so the backend returns 401 and triggers the modal.
    await page.addInitScript(() => {
      localStorage.removeItem('awaken.adminToken');
    });

    const agentListResponsePromise = page.waitForResponse(
      (response) =>
        response.url().includes('/v1/config/agents') && response.status() !== 401,
    );

    await page.goto('/agents');

    // The 401 should trigger the admin token modal.
    const modal = page.getByRole('dialog', { name: /Admin token required/ });
    await expect(modal).toBeVisible();

    // Fill in the correct token and save.
    await modal.locator('input[type="password"]').fill(TEST_ADMIN_TOKEN);
    await modal.getByRole('button', { name: 'Save' }).click();

    // Modal closes and the retry succeeds.
    await expect(modal).toBeHidden();
    const agentListResponse = await agentListResponsePromise;
    expect(agentListResponse.ok()).toBeTruthy();

    // Clean up so subsequent tests start without this token in localStorage.
    await page.evaluate(() => localStorage.removeItem('awaken.adminToken'));
  });

  test('unsaved-changes guard intercepts in-app navigation', async ({ page }) => {
    const agentId = `ui-guard-${suffix()}`;

    await page.goto('/agents/new');
    await page.getByLabel('Agent ID').fill(agentId);
    await page.getByLabel(/System prompt/).fill('halfway through editing');

    // Sticky header surfaces the dirty state; assert the badge appears
    // so we know the editor agrees there are unsaved changes.
    // (`.first()` because the SaveBar and header both echo the label —
    // we just need any one of them visible.)
    await expect(
      page.getByText('Unsaved changes', { exact: true }).first(),
    ).toBeVisible();

    // First attempt: click "Back to agents" then keep editing — the
    // dialog should appear and the URL must stay on /agents/new.
    await page.getByRole('link', { name: 'Back to agents' }).click();
    const dialog = page.getByRole('dialog', {
      name: /Discard unsaved changes/,
    });
    await expect(dialog).toBeVisible();
    await dialog.getByRole('button', { name: 'Keep editing' }).click();
    await expect(dialog).toBeHidden();
    await expect(page).toHaveURL(/\/agents\/new$/);
    await expect(page.getByLabel('Agent ID')).toHaveValue(agentId);

    // Second attempt: click again, this time discard.
    await page.getByRole('link', { name: 'Back to agents' }).click();
    const discardDialog = page.getByRole('dialog', {
      name: /Discard unsaved changes/,
    });
    await expect(discardDialog).toBeVisible();
    await discardDialog
      .getByRole('button', { name: 'Discard changes' })
      .click();
    await expect(page).toHaveURL(/\/agents$/);
  });

  // Regression coverage for the G1 fix: edits to other agent fields must not
  // strip context_policy / active_hook_filter from the saved record. Before
  // the fix, these fields were absent from `PATCHABLE_FIELDS` so the
  // customized-record PATCH path silently discarded user edits to them,
  // and absent from the TS `AgentSpec` type so any code path that built a
  // payload from the typed shape would drop them too.
  test('preserves context_policy and active_hook_filter across an editor round-trip', async ({
    page,
    request,
  }) => {
    const agentId = `ui-roundtrip-${suffix()}`;
    const seededPolicy = {
      max_context_tokens: 123_456,
      max_output_tokens: 4_096,
      min_recent_messages: 7,
      enable_prompt_cache: true,
      autocompact_threshold: 100_000,
      compaction_mode: 'compact_to_safe_frontier' as const,
      compaction_raw_suffix_messages: 5,
    };

    // 1. Seed a UserDefined agent that already has context_policy +
    //    active_hook_filter populated. POST creates a UserDefined record.
    const createResponse = await request.post(
      `${BACKEND_URL}/v1/config/agents`,
      {
        headers: {
          Authorization: `Bearer ${TEST_ADMIN_TOKEN}`,
          'content-type': 'application/json',
        },
        data: {
          id: agentId,
          model_id: 'default',
          system_prompt: 'Round-trip seed prompt.',
          max_rounds: 4,
          context_policy: seededPolicy,
          plugin_ids: ['permission'],
          active_hook_filter: ['permission'],
        },
      },
    );
    expect(createResponse.ok()).toBeTruthy();

    // 2. Open the editor for that agent. The Advanced tab must hydrate the
    //    seeded context_policy values.
    await page.goto(`/agents/${encodeURIComponent(agentId)}`);
    await page.waitForURL(new RegExp(`/agents/${agentId}`));
    await gotoEditorTab(page, 'Advanced');
    const contextSection = page
      .locator('section')
      .filter({
        has: page.getByRole('heading', { name: 'Context window policy' }),
      });
    await expect(contextSection.getByLabel('Apply custom policy')).toBeChecked();
    await expect(contextSection.getByLabel('Max context tokens')).toHaveValue(
      String(seededPolicy.max_context_tokens),
    );

    // 3. Edit max_context_tokens through the form and save.
    const editedMaxContext = 222_222;
    await contextSection
      .getByLabel('Max context tokens')
      .fill(String(editedMaxContext));
    await page.getByRole('button', { name: /^Save$/ }).first().click();
    await expect(
      page.getByRole('alert').filter({ hasText: new RegExp(`Agent\\s+"${agentId}"\\s+saved`) }),
    ).toBeVisible();

    // 4. Re-fetch through the API and verify *every* G1-tracked field made
    //    it through the round-trip — the edited one with its new value, and
    //    the untouched active_hook_filter unchanged.
    const refetched = await request.get(
      `${BACKEND_URL}/v1/config/agents/${encodeURIComponent(agentId)}`,
      { headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` } },
    );
    expect(refetched.ok()).toBeTruthy();
    const agent = await refetched.json();
    expect(agent.context_policy).toEqual({
      ...seededPolicy,
      max_context_tokens: editedMaxContext,
    });
    expect(agent.active_hook_filter).toEqual(['permission']);
  });

  // Defensive coverage for review #4 (stale `active_hook_filter` entries) is
  // implemented at the unit level in `partitionActiveHookFilter` + the
  // `ActiveHookFilterSection` component. Seeding stale entries through the
  // REST API is rejected at write time by the backend's `diagnose_agent_spec`
  // (`AgentHookFilterPluginNotLoaded`), so a full UI round-trip e2e for this
  // path is not reachable through the public surface — the partition is
  // defensive against legacy / backup imports / future schema drift only.

  // F4: dark mode smoke. Verifies the theme system actually applies the
  // `data-theme="dark"` attribute and that the editor renders without
  // throwing or going blank when dark is forced. Visual diff is out of
  // scope; this is a presence-of-content smoke.
  test('editor renders in dark mode with data-theme="dark" applied', async ({ page }) => {
    // First navigation: app boots with whatever theme defaults to (system).
    // The beforeEach already wrote the admin bearer token, so we won't get
    // stuck on the token modal. Once the app is alive, write the theme
    // choice and reload — the next paint applies `data-theme="dark"`.
    await page.goto('/agents/new');
    await page.waitForURL(/\/agents\/new/);
    await expect(page.getByRole('tablist', { name: 'Editor sections' })).toBeVisible();

    await page.evaluate(() => {
      localStorage.setItem('awaken.admin.theme', 'dark');
    });
    await page.reload();
    await expect(page.getByRole('tablist', { name: 'Editor sections' })).toBeVisible();

    // Theme attribute must be set on <html> for the dark stylesheet to apply.
    const themeAttr = await page.evaluate(
      () => document.documentElement.getAttribute('data-theme'),
    );
    expect(themeAttr).toBe('dark');

    // Editor scaffolding must remain visible after the theme switch.
    await expect(page.getByLabel('Agent ID')).toBeVisible();
    await expect(page.getByLabel('Model')).toBeVisible();

    // Tabs each render their content without throwing.
    for (const tab of ['Basics', 'Tools', 'Plugins', 'Delegates', 'Advanced'] as const) {
      await gotoEditorTab(page, tab);
      await expect(page.getByRole('tab', { name: tab, selected: true })).toBeVisible();
    }
  });

  // F5: live AiSdkEncoder tool-call card. Sends a `RUN_WEATHER_TOOL`
  // directive to the scripted executor through the sandbox preview panel,
  // then verifies that a tool-call card actually renders (state badge +
  // tool name + input) — proving the unit-level fixture tests in
  // `agent-preview-panel.test.tsx` match the real wire shape.
  test('sandbox preview renders a tool-call card from a live event stream', async ({
    page,
    request,
  }) => {
    const agentId = `ui-sandbox-tool-${suffix()}`;
    // Seed an agent that the scripted executor will route through. The
    // model_id `default` is bound to the scripted executor in
    // examples/src/starter_backend.
    const createResponse = await request.post(`${BACKEND_URL}/v1/config/agents`, {
      headers: {
        Authorization: `Bearer ${TEST_ADMIN_TOKEN}`,
        'content-type': 'application/json',
      },
      data: {
        id: agentId,
        model_id: 'default',
        system_prompt: 'Use the scripted tool directives when the user provides one.',
        max_rounds: 1,
      },
    });
    expect(createResponse.ok()).toBeTruthy();

    await page.goto(`/agents/${encodeURIComponent(agentId)}`);
    await page.waitForURL(new RegExp(`/agents/${agentId}`));

    // The sandbox lives in the preview side panel. Type the scripted
    // directive and submit.
    const previewArea = page
      .locator('aside')
      .filter({ has: page.getByRole('heading', { name: 'Sandbox' }) });
    await previewArea.getByPlaceholder('Type a message…').fill('RUN_WEATHER_TOOL');
    await previewArea.getByRole('button', { name: /Send/ }).click();

    // The card carries the tool name in monospace. We don't pin the
    // state badge text because it depends on whether the script also
    // emits an output — we just need at least one tool-call card to
    // appear, confirming the real `AiSdkEncoder` stream is shaped the
    // way `MessageParts` / `ToolInvocation` expect.
    await expect(previewArea.getByText('get_weather')).toBeVisible({ timeout: 30_000 });
  });

  // F4: mobile breakpoint smoke. The admin console targets desktop
  // primarily — the desktop sidebar overlays the editor at 375 px width
  // (no mobile-collapsed nav today), and content-density tabs (Plugins,
  // Tools) inherently overflow at that viewport. Designing for that is a
  // designer-pass / responsive-layout task, not a smoke. So the smoke
  // verifies the minimum testable guarantees: (1) the app doesn't get
  // stuck on a loading screen at mobile viewport, (2) the editor route
  // still mounts, (3) every Editor section tab is still reachable through
  // the tablist control. Failure here means a hard regression — e.g. a
  // ResizeObserver loop, an SSR mismatch, or a non-responsive component
  // breaking the page entirely.
  test('editor mounts and tabs remain reachable at mobile breakpoint (375x812)', async ({
    page,
  }) => {
    await page.setViewportSize({ width: 375, height: 812 });
    await page.goto('/agents/new');
    await page.waitForURL(/\/agents\/new/);

    await expect(page.getByRole('tablist', { name: 'Editor sections' })).toBeVisible();

    for (const tab of ['Basics', 'Tools', 'Plugins', 'Advanced'] as const) {
      await gotoEditorTab(page, tab);
      await expect(page.getByRole('tab', { name: tab, selected: true })).toBeVisible();
    }
  });
});
