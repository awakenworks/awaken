import { appendFileSync } from 'node:fs';

const receiptFile = process.env.AWAKEN_MANAGED_SDK_RECEIPT_FILE;
if (!receiptFile) throw new Error('AWAKEN_MANAGED_SDK_RECEIPT_FILE is required');

const originalFetch = globalThis.fetch;
if (typeof originalFetch !== 'function') throw new Error('global fetch is required');

globalThis.fetch = async function receiptFetch(input, init) {
  const request = new Request(input, init);
  const url = new URL(request.url);
  const betas = (request.headers.get('anthropic-beta') ?? '')
    .split(',')
    .map((value) => value.trim())
    .filter(Boolean);
  const response = await originalFetch.call(globalThis, input, init);
  appendFileSync(receiptFile, `${JSON.stringify({
    method: request.method,
    path: url.pathname,
    beta: url.searchParams.get('beta'),
    betas,
    sdk: request.headers.get('x-stainless-lang') === 'js',
    status: response.status,
  })}\n`);
  return response;
};
