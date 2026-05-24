import { expect, test } from '@playwright/test';
import { TEST_ADMIN_TOKEN } from '../playwright.admin.config';

const BACKEND_URL = process.env.AWAKEN_BACKEND_URL ?? 'http://127.0.0.1:38080';

function uniqueId(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function authHeaders() {
  return { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` };
}

async function expectNoServerError(
  label: string,
  response: Awaited<ReturnType<import('@playwright/test').APIRequestContext['get']>>,
) {
  expect(response.status(), label).toBeLessThan(500);
}

test.describe('admin API surface coverage', () => {
  test('covers capabilities, config CRUD, overrides, diagnostics, stats, and audit routes', async ({
    request,
  }) => {
    const providerId = uniqueId('api-provider');
    const modelId = uniqueId('api-model');
    const agentId = uniqueId('api-agent');
    const headers = authHeaders();

    const capabilityChecks = [
      ['GET /v1/capabilities', request.get(`${BACKEND_URL}/v1/capabilities`, { headers })],
      ['GET /v1/system/info', request.get(`${BACKEND_URL}/v1/system/info`, { headers })],
      [
        'GET /v1/agents/runtime-stats',
        request.get(`${BACKEND_URL}/v1/agents/runtime-stats`, { headers }),
      ],
      [
        'GET /v1/agents/:id/runtime-stats',
        request.get(`${BACKEND_URL}/v1/agents/default/runtime-stats`, { headers }),
      ],
      [
        'GET /v1/config/diagnostics',
        request.get(`${BACKEND_URL}/v1/config/diagnostics`, { headers }),
      ],
      ['GET /v1/audit-log', request.get(`${BACKEND_URL}/v1/audit-log`, { headers })],
      ['GET /v1/agents', request.get(`${BACKEND_URL}/v1/agents`, { headers })],
      ['GET /v1/agents/:id', request.get(`${BACKEND_URL}/v1/agents/default`, { headers })],
    ] as const;

    for (const [label, pending] of capabilityChecks) {
      await expectNoServerError(label, await pending);
    }

    const providerPayload = { id: providerId, adapter: 'openai', timeout_secs: 30 };
    const providerValidate = await request.post(`${BACKEND_URL}/v1/config/providers/validate`, {
      headers,
      data: providerPayload,
    });
    expect(providerValidate.ok(), 'POST /v1/config/:namespace/validate').toBeTruthy();

    const providerCreate = await request.post(`${BACKEND_URL}/v1/config/providers`, {
      headers,
      data: providerPayload,
    });
    expect(providerCreate.status(), 'POST /v1/config/:namespace').toBe(201);

    // Exercise the capability/pricing fields on the create path so this
    // suite covers the post-unification ModelSpec wire shape end-to-end.
    const modelPayload = {
      id: modelId,
      provider_id: providerId,
      upstream_model: 'coverage-model',
      context_window: 200_000,
      max_output_tokens: 16_384,
      modalities: { input: ['text'], output: ['text'] },
      knowledge_cutoff: '2026-01',
      input_token_price_per_million_usd: 3,
      output_token_price_per_million_usd: 15,
    };
    const modelCreate = await request.post(`${BACKEND_URL}/v1/config/models`, {
      headers,
      data: modelPayload,
    });
    expect(modelCreate.status(), 'POST /v1/config/models').toBe(201);

    // Read-back round-trip: every capability field must persist verbatim.
    const modelRead = await request.get(`${BACKEND_URL}/v1/config/models/${modelId}`, {
      headers,
    });
    expect(modelRead.ok(), 'GET /v1/config/models/:id').toBeTruthy();
    const modelReadBody = (await modelRead.json()) as Record<string, unknown>;
    expect(modelReadBody.context_window, 'context_window round-trip').toBe(200_000);
    expect(modelReadBody.max_output_tokens, 'max_output_tokens round-trip').toBe(16_384);
    expect(modelReadBody.knowledge_cutoff, 'knowledge_cutoff round-trip').toBe('2026-01');
    expect(modelReadBody.modalities, 'modalities round-trip').toEqual({
      input: ['text'],
      output: ['text'],
    });
    expect(modelReadBody.input_token_price_per_million_usd, 'input price round-trip').toBe(3);
    expect(modelReadBody.output_token_price_per_million_usd, 'output price round-trip').toBe(15);

    // Validation: max_output_tokens > context_window is rejected by the
    // server-side `validate_model_spec`. Use a fresh id so the failure isn't
    // confused with a duplicate-id 409.
    const invalidModelPayload = {
      id: `${modelId}-invalid`,
      provider_id: providerId,
      upstream_model: 'coverage-model',
      context_window: 1000,
      max_output_tokens: 2000,
    };
    const invalidModelCreate = await request.post(`${BACKEND_URL}/v1/config/models`, {
      headers,
      data: invalidModelPayload,
    });
    expect(invalidModelCreate.status(), 'POST invalid model spec').toBe(400);

    // Duplicate-id rejection on a second create with the same id.
    const duplicateModelCreate = await request.post(`${BACKEND_URL}/v1/config/models`, {
      headers,
      data: modelPayload,
    });
    expect(duplicateModelCreate.status(), 'POST duplicate model id').toBe(409);

    const agentPayload = {
      id: agentId,
      model_id: modelId,
      system_prompt: 'Admin API coverage agent',
      max_rounds: 1,
    };
    const agentCreate = await request.post(`${BACKEND_URL}/v1/config/agents`, {
      headers,
      data: agentPayload,
    });
    expect(agentCreate.status(), 'POST /v1/config/agents').toBe(201);

    const configChecks = [
      [
        'GET /v1/config/:namespace',
        request.get(`${BACKEND_URL}/v1/config/providers?limit=20&offset=0`, { headers }),
      ],
      [
        'GET /v1/config/:namespace/:id',
        request.get(`${BACKEND_URL}/v1/config/providers/${providerId}`, { headers }),
      ],
      [
        'PUT /v1/config/:namespace/:id',
        request.put(`${BACKEND_URL}/v1/config/providers/${providerId}`, {
          headers,
          data: { ...providerPayload, timeout_secs: 31 },
        }),
      ],
      [
        'GET /v1/config/:namespace/:id/meta',
        request.get(`${BACKEND_URL}/v1/config/providers/${providerId}/meta`, { headers }),
      ],
      [
        'GET /v1/config/:namespace/meta',
        request.get(`${BACKEND_URL}/v1/config/providers/meta?limit=20&offset=0`, { headers }),
      ],
      [
        'GET /v1/config/:namespace/$schema',
        request.get(`${BACKEND_URL}/v1/config/providers/$schema`, { headers }),
      ],
      [
        'POST /v1/config/:namespace/:id/restore',
        request.post(`${BACKEND_URL}/v1/config/providers/${providerId}/restore`, {
          headers,
          data: { version: 'missing-version-for-route-coverage' },
        }),
      ],
      [
        'GET /v1/config/providers/:id/removal-preview',
        request.get(`${BACKEND_URL}/v1/config/providers/${providerId}/removal-preview`, {
          headers,
        }),
      ],
      [
        'POST /v1/providers/:id/test',
        request.post(`${BACKEND_URL}/v1/providers/${providerId}/test`, { headers }),
      ],
    ] as const;

    for (const [label, pending] of configChecks) {
      await expectNoServerError(label, await pending);
    }

    const overrideChecks = [
      [
        'PATCH /v1/config/agents/:id/overrides',
        request.patch(`${BACKEND_URL}/v1/config/agents/${agentId}/overrides`, {
          headers,
          data: { system_prompt: 'override route coverage' },
        }),
      ],
      [
        'DELETE /v1/config/agents/:id/overrides/:field',
        request.delete(`${BACKEND_URL}/v1/config/agents/${agentId}/overrides/system_prompt`, {
          headers,
        }),
      ],
      [
        'DELETE /v1/config/agents/:id/overrides',
        request.delete(`${BACKEND_URL}/v1/config/agents/${agentId}/overrides`, { headers }),
      ],
      [
        'PATCH /v1/config/tools/:id/overrides',
        request.patch(`${BACKEND_URL}/v1/config/tools/get_weather/overrides`, {
          headers,
          data: { description: 'weather override coverage' },
        }),
      ],
      [
        'DELETE /v1/config/tools/:id/overrides/:field',
        request.delete(`${BACKEND_URL}/v1/config/tools/get_weather/overrides/description`, {
          headers,
        }),
      ],
      [
        'DELETE /v1/config/tools/:id/overrides',
        request.delete(`${BACKEND_URL}/v1/config/tools/get_weather/overrides`, { headers }),
      ],
    ] as const;

    for (const [label, pending] of overrideChecks) {
      await expectNoServerError(label, await pending);
    }

    const deleteAgent = await request.delete(`${BACKEND_URL}/v1/config/agents/${agentId}`, {
      headers,
    });
    expect(deleteAgent.status(), 'DELETE /v1/config/agents/:id').toBe(204);
    const deleteModel = await request.delete(`${BACKEND_URL}/v1/config/models/${modelId}`, {
      headers,
    });
    expect(deleteModel.status(), 'DELETE /v1/config/models/:id').toBe(204);
    const deleteProvider = await request.delete(
      `${BACKEND_URL}/v1/config/providers/${providerId}?force=true`,
      { headers },
    );
    expect(deleteProvider.status(), 'DELETE /v1/config/providers/:id').toBe(204);
  });
});
