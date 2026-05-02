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
  name: 'Basics' | 'Tools' | 'Plugins' | 'Delegates' | 'Advanced',
) {
  await page
    .getByRole('tablist', { name: 'Editor sections' })
    .getByRole('tab', { name })
    .click();
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
    await expect(page.locator('pre')).toContainText('"default_behavior": "deny"');
    await expect(page.locator('pre')).not.toContainText('deferred_tools');

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
    await expect(page.locator('pre')).toContainText('"deferred_tools"');
    await expect(page.locator('pre')).toContainText('"beta_overhead": 0');

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
    await expect(
      page.getByText('Unsaved changes', { exact: true }),
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
});
