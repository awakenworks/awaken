// Managed content-block compatibility through the official @anthropic-ai/sdk.
//
// Cause/effect graph:
// C1 uploaded image/document belongs to the request Workspace;
// C2 user.message references both Files plus a payloadless redacted block;
// C3 an awaiting client tool receives image/document/search-result content;
// C4 Runtime dispatch reaches the Anthropic Messages ACL;
// E1 SDK receipts/history retain the official File source shape;
// E2 each File is resolved once through the Files authority and the provider sees
//    base64 with the catalog MIME, never an Awaken file_id;
// E3 redacted content is retained in Managed history but omitted from provider I/O;
// E4 tool correlation, nested media, search citations, and terminal reply survive.
// Constraint: C3 is legal only after the matching agent.custom_tool_use.
//
// Decision table:
// R1 C1+C2+C4 -> E1+E2+E3; R2 C1+C2+C3+C4 -> E1+E2+E4.
//
// FMECA:
// - logical file_id leaks to a provider (critical): attempt-bound materializer and
//   the sanitized upstream structural capture make R1/R2 fail;
// - MIME/digest mismatch (critical): Files metadata verification fails the turn;
// - rich ToolResult flattened to text (high): R2 asserts the provider's closed
//   image/document/search_result union and citations;
// - redacted payload fabricated or exposed (critical): payloadless domain variant
//   plus R1's history/provider split;
// - tool result attached to the wrong call (high): official SDK type plus pending
//   tool-use admission and R2's terminal response.

import assert from 'node:assert/strict';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import type {
  BetaManagedAgentsUserMessageEventParams,
  BetaManagedAgentsUserToolResultEventParams,
} from '@anthropic-ai/sdk/resources/beta/sessions/events';
// @ts-ignore -- shared JS harness deliberately serves both JS and TS scenarios.
import { RED_PNG_B64, withScenarioServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = Number(process.env.E2E_PORT ?? 38248);

type Shape = {
  type: string;
  source_type?: string;
  media_type?: string;
  citations_enabled?: boolean;
  result_parts?: number;
  content?: Shape[];
  content_container?: string;
  content_length?: number;
};

type ProviderRequest = {
  contentShape: Array<{ role: string; content: Shape[] }>;
};

async function events(client: Anthropic, sessionId: string) {
  const values = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    values.push(event);
  }
  return values;
}

function latestToolResultShape(request: ProviderRequest): Shape | undefined {
  return request.contentShape
    .flatMap((message) => message.content)
    .findLast((block) => block.type === 'tool_result');
}

async function main() {
  await withScenarioServer(
    'custom',
    'custom',
    PORT,
    async (baseUrl: string, upstream: { requests: ProviderRequest[] }) => {
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
    const image = await client.beta.files.upload({
      file: await toFile(Buffer.from(RED_PNG_B64, 'base64'), 'red.png', { type: 'image/png' }),
      betas: BETAS,
    });
    const document = await client.beta.files.upload({
      file: await toFile(Buffer.from('AWAKEN_TYPED_DOCUMENT'), 'facts.txt', { type: 'text/plain' }),
      betas: BETAS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });

    const message: BetaManagedAgentsUserMessageEventParams = {
      type: 'user.message',
      content: [
        { type: 'image', source: { type: 'file', file_id: image.id } },
        {
          type: 'document',
          source: { type: 'file', file_id: document.id },
          title: 'Facts',
          context: 'Typed content compatibility fixture',
        },
        { type: 'redacted' },
        { type: 'text', text: 'Use the configured client tool.' },
      ],
    };
    await client.beta.sessions.events.send(session.id, { events: [message], betas: BETAS });

    let history = await events(client, session.id);
    const toolUse = history.find((event) => event.type === 'agent.custom_tool_use');
    assert.ok(toolUse, `R1 expected custom tool use: ${history.map((event) => event.type)}`);

    const firstProvider = upstream.requests.at(-1) as ProviderRequest;
    const firstBlocks = firstProvider.contentShape.flatMap((item) => item.content);
    assert.ok(firstBlocks.some((block) =>
      block.type === 'image' && block.source_type === 'base64' && block.media_type === 'image/png'
    ), 'R1/E2 image File is materialized');
    assert.ok(firstBlocks.some((block) =>
      block.type === 'document' && block.source_type === 'text' && block.media_type === 'text/plain'
    ), 'R1/E2 document File is materialized');
    assert.ok(!firstBlocks.some((block) => block.source_type === 'file'), 'R1/E2 no logical File reaches provider');
    assert.ok(!firstBlocks.some((block) => block.type === 'redacted'), 'R1/E3 redacted is not provider-visible');

    const firstUser = history.find((event) => event.type === 'user.message');
    assert.ok(firstUser?.content.some((block) =>
      block.type === 'document' && block.source.type === 'file' && block.source.file_id === document.id
    ), 'R1/E1 Managed history retains official File source');
    assert.ok(firstUser?.content.some((block) => block.type === 'redacted'), 'R1/E3 Managed history retains redaction');

    const result: BetaManagedAgentsUserToolResultEventParams = {
      type: 'user.tool_result',
      tool_use_id: toolUse.id,
      content: [
        { type: 'text', text: 'RICH_TOOL_RESULT' },
        { type: 'image', source: { type: 'file', file_id: image.id } },
        { type: 'document', source: { type: 'file', file_id: document.id }, title: 'Facts' },
        {
          type: 'search_result',
          source: 'https://example.test/facts',
          title: 'Typed result',
          content: [{ type: 'text', text: 'Search evidence' }],
          citations: { enabled: true },
        },
      ],
    };
    await client.beta.sessions.events.send(session.id, { events: [result], betas: BETAS });

    history = await events(client, session.id);
    const retainedResult = history.find((event) => event.type === 'user.tool_result');
    assert.deepEqual(
      retainedResult?.content?.map((block) => block.type),
      ['text', 'image', 'document', 'search_result'],
      `R2/E1 Managed history retains rich content: ${JSON.stringify(retainedResult)}`,
    );
    const secondProvider = upstream.requests.at(-1) as ProviderRequest;
    const toolResult = latestToolResultShape(secondProvider);
    assert.ok(toolResult, 'R2/E4 correlated tool result reaches provider');
    assert.deepEqual(
      toolResult.content?.map((block) => block.type),
      ['text', 'image', 'document', 'search_result'],
      `R2/E2/E4 rich ToolResult remains typed: ${JSON.stringify(secondProvider.contentShape)}`,
    );
    assert.deepEqual(
      toolResult.content?.filter((block) => ['image', 'document'].includes(block.type))
        .map((block) => block.source_type),
      ['base64', 'text'],
      'R2/E2 nested Files materialize with MIME-correct source variants',
    );
    const search = toolResult.content?.find((block) => block.type === 'search_result');
    assert.deepEqual(
      { citations: search?.citations_enabled, parts: search?.result_parts },
      { citations: true, parts: 1 },
      'R2/E4 search citations survive',
    );
    const finalText = history
      .filter((event) => event.type === 'agent.message')
      .flatMap((event) => event.content)
      .filter((block) => block.type === 'text')
      .map((block) => block.text)
      .join('\n');
    assert.match(finalText, /RICH_TOOL_RESULT/u, 'R2/E4 correlated turn reaches terminal answer');
    console.log('E2E PASS: official Anthropic SDK typed content blocks and Files materialization.');
    },
  );
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
