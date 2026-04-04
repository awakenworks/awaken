import { test, expect } from '@playwright/test';
import { aiSdkFilePart, aiSdkMessage, aiSdkTextPart } from './ai-sdk-test-utils';

test.describe('AG-UI multimodal input', () => {
  test('text string backward compatible', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello plain text' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('multimodal content array with image URL', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{
          role: 'user',
          content: [
            { type: 'text', text: 'Describe this image' },
            { type: 'image', source: { type: 'url', value: 'https://example.com/photo.png' } },
          ],
        }],
      },
    });
    expect(res.status()).toBeLessThan(500);
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('base64 image (data source)', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{
          role: 'user',
          content: [
            { type: 'text', text: 'What is in this image?' },
            {
              type: 'image',
              source: {
                type: 'data',
                value: 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
                mimeType: 'image/png',
              },
            },
          ],
        }],
      },
    });
    expect(res.status()).toBeLessThan(500);
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('audio content', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{
          role: 'user',
          content: [
            { type: 'text', text: 'Transcribe this audio' },
            {
              type: 'audio',
              source: { type: 'url', value: 'https://example.com/clip.mp3' },
            },
          ],
        }],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });

  test('document content', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{
          role: 'user',
          content: [
            { type: 'text', text: 'Summarize this doc' },
            {
              type: 'document',
              source: {
                type: 'data',
                value: 'JVBERi0xLjQK',
                mimeType: 'application/pdf',
              },
            },
          ],
        }],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });

  test('video content', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{
          role: 'user',
          content: [
            { type: 'text', text: 'Describe this video' },
            {
              type: 'video',
              source: { type: 'url', value: 'https://example.com/clip.mp4' },
            },
          ],
        }],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });
});

test.describe('AI SDK multimodal', () => {
  test('multimodal message via AI SDK', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: [
          aiSdkMessage('user', [
            aiSdkTextPart('What do you see?'),
            aiSdkFilePart('https://example.com/photo.png', 'image/png'),
          ]),
        ],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });
});

test.describe('/v1/runs multimodal', () => {
  test('accepts multimodal content array', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{
          role: 'user',
          content: [
            { type: 'text', text: 'Analyze this' },
            { type: 'image_url', url: 'https://example.com/photo.png' },
          ],
        }],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });
});
