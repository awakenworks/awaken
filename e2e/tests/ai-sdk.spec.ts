import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

test('AI SDK chat endpoint returns SSE stream', async ({ request }) => {
  const response = await request.post('/v1/ai-sdk/chat', {
    data: {
      agentId: 'default',
      messages: aiSdkTextMessages([{ role: 'user', text: 'What is the weather?' }]),
    },
  });
  expect(response.ok()).toBeTruthy();
  const body = await response.text();
  expect(body.length).toBeGreaterThan(0);
});
