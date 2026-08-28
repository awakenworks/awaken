import { appendFileSync } from 'node:fs';
import { recordingFetch } from './managed_sdk_operation_receipts.mjs';

const receiptFile = process.env.AWAKEN_MANAGED_SDK_RECEIPT_FILE;
if (!receiptFile) throw new Error('AWAKEN_MANAGED_SDK_RECEIPT_FILE is required');

const originalFetch = globalThis.fetch;
if (typeof originalFetch !== 'function') throw new Error('global fetch is required');

globalThis.fetch = recordingFetch(
  originalFetch.bind(globalThis),
  (receipt) => appendFileSync(receiptFile, `${JSON.stringify(receipt)}\n`),
);
