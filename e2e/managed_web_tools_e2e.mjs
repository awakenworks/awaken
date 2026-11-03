// Managed Web execution through the official Anthropic SDK Session wire.
// Config authoring is raw HTTP only because the official Agent DTO has no
// plugins/plugin_config fields; publication, Session creation, Events, tool
// execution, and terminal observation all run through the production AllInOne.

import assert from 'node:assert/strict';
import http from 'node:http';
import Anthropic from '@anthropic-ai/sdk';
import { pass, waitForSessionEventReceipt, withScenarioServer } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38209);
const BETAS = ['managed-agents-2026-04-01'];
const SEARCH_PROVIDER = 'scenario-web-search';
const SEARCH_SECRET = 'scenario-search-secret'; // awaken-allow(secret): (deterministic fixture)

async function startFetchFixture() {
  const requests = [];
  const server = http.createServer((request, response) => {
    requests.push(request.url);
    if (request.url === '/doc.PDF') {
      response.writeHead(200, { 'content-type': 'application/pdf' });
    } else {
      response.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    }
    response.end('abcdefghijklmnop');
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  return {
    baseURL: `http://127.0.0.1:${address.port}`,
    requests,
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

function eventText(event) {
  return (event?.content ?? []).map((block) => block.text ?? '').join('');
}

async function configRequest(baseURL, method, path, body) {
  const response = await fetch(`${baseURL}${path}`, {
    method,
    headers: {
      'anthropic-beta': BETAS[0],
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function publishAgent(baseURL, id, body) {
  let response = await configRequest(baseURL, 'PUT', `/v1/config/agents/${id}`, body);
  assert.equal(response.status, 200, `store ${id}: ${JSON.stringify(response.body)}`);
  response = await configRequest(baseURL, 'POST', `/v1/config/agents/${id}/validate`, body);
  assert.equal(response.status, 200, `validate ${id}: ${JSON.stringify(response.body)}`);
  assert.equal(response.body.valid, true, `valid ${id}: ${JSON.stringify(response.body)}`);
  response = await configRequest(baseURL, 'POST', `/v1/config/agents/${id}/publish`, undefined);
  assert.equal(response.status, 200, `publish ${id}: ${JSON.stringify(response.body)}`);
  assert.equal(response.body.installed, true, `${id} installed into the live catalog`);
}

async function runToolTurn(client, agent, prompt, expectedTool, expectedSources) {
  const session = await client.beta.sessions.create({
    agent,
    environment_id: 'env_local',
    betas: BETAS,
  });
  if (expectedSources) {
    const config = session.agent.tools.flatMap((tool) => tool.configs ?? [])
      .find(({ name }) => name === 'web_fetch');
    assert.deepEqual(config.url_sources, expectedSources, 'one resolved SDK response normalizes source policy');
  }
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: prompt }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', `${expectedTool} exact User receipt`);
  const { delta } = await waitForSessionEventReceipt(
    client,
    session.id,
    receiptId,
    BETAS,
    ({ delta: current }) => {
      const use = current.find((event) =>
        event.type === 'agent.tool_use' && event.name === expectedTool);
      return use
        && current.some((event) =>
          event.type === 'agent.tool_result' && event.tool_use_id === use.id)
        && current.some((event) => event.type === 'agent.message')
        && [...current].reverse().find((event) =>
          event.type === 'session.status_idle')?.stop_reason?.type === 'end_turn';
    },
    `${expectedTool} to execute and settle one Run`,
  );
  const uses = delta.filter((event) =>
    event.type === 'agent.tool_use' && event.name === expectedTool);
  assert.equal(uses.length, 1, `${expectedTool} has one durable tool_use`);
  const results = delta.filter((event) =>
    event.type === 'agent.tool_result' && event.tool_use_id === uses[0].id);
  assert.equal(results.length, 1, `${expectedTool} has one linked durable tool_result`);
  const messages = delta.filter((event) => event.type === 'agent.message');
  assert.equal(messages.length, 1, `${expectedTool} has one terminal model reply`);
  assert.equal(
    [...delta].reverse().find((event) => event.type === 'session.status_idle')?.stop_reason?.type,
    'end_turn',
    `${expectedTool} ends idle instead of crashing the Session`,
  );
  return { delta, result: results[0], message: messages[0] };
}

async function main() {
  const fixture = await startFetchFixture();
  try {
    // R0 cause/effect: the management-web test-support AllInOne has a typed
    // Deployment but no caller-supplied RuntimeAuthority. Its composition
    // selects one scenario authority before both Host and SessionApplication
    // bind Dispatch; startup and the Web turns below must both succeed.
    await withScenarioServer('management-web', 'default', PORT, async (baseURL) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
      const model = {
        mode: 'pinned',
        provider_identity_ref: 'default',
        model_ref: 'management-web',
        backend_ref: 'default',
      };
      const alwaysAllow = { enabled: true, permission_policy: { type: 'always_allow' } };

      const context = await configRequest(baseURL, 'GET', '/v1/config/workspace-context');
      assert.equal(context.status, 200, 'the composition root exposes its Workspace');
      assert.equal(typeof context.body.workspace_id, 'string');
      assert.ok(context.body.workspace_id.length > 0, 'one authoritative Workspace');
      const credentialInput = {
        idempotency_key: 'managed-web-tools-e2e-search',
        workspace_id: context.body.workspace_id,
        kind: 'vault',
        env_key: null,
        secret: SEARCH_SECRET,
        // Credential cause/effect row C0: an exact Web-provider descriptor binds
        // the one opaque material to provider+target+usage at creation. Effect:
        // later materialization may satisfy C6; a generic legacy provider source
        // must fail before plaintext because it does not authorize this target.
        descriptor: {
          provider: SEARCH_PROVIDER,
          material: { kind: 'secret', type_id: 'awaken.secret/v1' },
          targets: [{
            target: {
              purpose: { type: 'web_provider_authorization' },
              audience: SEARCH_PROVIDER,
            },
            usage: {
              type: 'http_header',
              name: 'X-Scenario-Web-Key',
            },
          }],
        },
      };
      // Scope cause/effect table: W1 a foreign body claim => 404 before sealing;
      // W2 the exact edge-resolved Workspace with the same idempotency identity
      // => 201 and secret-free output. Reusing that identity positively proves
      // the refused claim did not poison the successful authoring operation.
      const foreign = await configRequest(baseURL, 'POST', '/v1/config/credentials', {
        ...credentialInput,
        workspace_id: `${context.body.workspace_id}-foreign`,
      });
      assert.equal(foreign.status, 404, 'W1 foreign Workspace claim is refused');
      assert.ok(!JSON.stringify(foreign.body).includes(SEARCH_SECRET), 'W1 refusal is secret-free');
      const credential = await configRequest(baseURL, 'POST', '/v1/config/credentials', credentialInput);
      assert.equal(credential.status, 201, JSON.stringify(credential.body));
      assert.ok(!JSON.stringify(credential.body).includes(SEARCH_SECRET), 'credential response is secret-free');

      const fetchDefinition = {
        name: 'Allowed Web fetch',
        system: 'Execute the requested Web tool and report its result.',
        max_steps: 4,
        model,
        tools: [{
          type: 'agent_toolset_20260401',
          configs: [{
            name: 'web_fetch',
            ...alwaysAllow,
            allowed_domains: ['127.0.0.1'],
            max_content_tokens: 3,
          }],
        }],
        plugins: ['web_fetch'],
        plugin_config: {},
      };
      const fetchWithSources = (url_sources) => ({ ...fetchDefinition, tools: [{
        ...fetchDefinition.tools[0], configs: [{ ...fetchDefinition.tools[0].configs[0], url_sources }],
      }] });
      const userSources = { client_tool_results: 'none', server_tool_results: 'none', user_input: 'all' };
      const blockedUserSources = { client_tool_results: 'all', server_tool_results: 'none', user_input: 'none' };
      await publishAgent(baseURL, 'web-fetch-allow', fetchWithSources(userSources));
      await publishAgent(baseURL, 'web-fetch-source-deny', fetchWithSources(blockedUserSources));
      await publishAgent(baseURL, 'web-policy-block-search', {
        name: 'Blocked fetch and configured search',
        system: 'Execute the requested Web tool and report its result.',
        max_steps: 4,
        model,
        tools: [{
          type: 'agent_toolset_20260401',
          configs: [{
            name: 'web_fetch',
            ...alwaysAllow,
            blocked_domains: ['127.0.0.1'],
          }, {
            name: 'web_search',
            ...alwaysAllow,
            blocked_domains: ['blocked.test'],
            user_location: {
              type: 'approximate',
              city: 'Shanghai',
              country: 'CN',
              region: 'Shanghai',
              timezone: 'Asia/Shanghai',
            },
          }],
        }],
        plugins: ['web_fetch', 'web_search'],
        plugin_config: {
          web_search: {
            provider_id: SEARCH_PROVIDER,
            credential: { id: credential.body.id, revision: credential.body.version },
            options: {},
          },
        },
      });

      // Fetch cause/effect graph:
      // C1 published allow policy+estimated cap3 and 16 ASCII fixture scalars;
      // C2 URL matches; C3 a `.PDF` URL returns the fixture's ASCII text;
      // C4 published block policy matches the combined domain policy. Effects: E1 physical HTTP once;
      // E2 non-PDF text=`abcdefghijkl`; E3 PDF-URL text has the SAME prefix; E4 block is an
      // error result before HTTP; E7 each Run durably links one use/result/final/idle.
      // Constraints K1 one publication snapshot selects the sole configured
      // WebFetch plugin, K3 that plugin owns fetch policy while the direct
      // provider retains only GET/lossy UTF-8/1MiB, K7 reject has zero I/O.
      // The existing owner estimates four ASCII scalars per token: cap3 allows
      // exactly 12 here. The 16-scalar body exceeds it, so removing the cap or
      // bypassing this text cap solely for a PDF URL fails exact literals, not
      // a short unchanged body. The application/pdf response is still pure ASCII
      // and the provider returns String. SDK binary-PDF exemption is separate:
      // this case proves neither binary-document parsing nor its token budget.
      // This is not a model-tokenizer or billing-count assertion.
      // Decision table: F1 C1+C2+!C3=>E1+E2+E7; F2 C1+C2+C3=>E1+E3+E7;
      // F3 C4=>E4+E7 with fixture request count unchanged.
      const beforeText = fixture.requests.length;
      const textFetch = await runToolTurn(
        client,
        'web-fetch-allow',
        `web-fetch ${fixture.baseURL}/text`,
        'web_fetch',
        { client_tool_results: { type: 'none' }, server_tool_results: { type: 'none' }, user_input: { type: 'all' } },
      );
      assert.equal(fixture.requests.length, beforeText + 1, 'F1/E1 exact HTTP side effect');
      assert.equal(fixture.requests.at(-1), '/text', 'F1 allowed target reached');
      assert.equal(textFetch.result.is_error, false, 'F1 successful result');
      assert.equal(eventText(textFetch.result), 'abcdefghijkl', 'F1/E2 cap applies to non-PDF text');
      assert.equal(eventText(textFetch.message), 'web-result: abcdefghijkl', 'F1 model observes capped result');

      const beforePdf = fixture.requests.length;
      const pdfFetch = await runToolTurn(
        client,
        'web-fetch-allow',
        `web-fetch ${fixture.baseURL}/doc.PDF`,
        'web_fetch',
      );
      assert.equal(fixture.requests.length, beforePdf + 1, 'F2/E1 exact HTTP side effect');
      assert.equal(fixture.requests.at(-1), '/doc.PDF', 'F2 uppercase PDF target reached');
      assert.equal(eventText(pdfFetch.result), 'abcdefghijkl', 'F2/E3 PDF uses the same estimated budget');
      assert.equal(eventText(pdfFetch.message), 'web-result: abcdefghijkl', 'F2 model observes the same capped PDF prefix');

      const beforeBlocked = fixture.requests.length;
      const blockedFetch = await runToolTurn(
        client,
        'web-policy-block-search',
        `web-fetch ${fixture.baseURL}/blocked`,
        'web_fetch',
      );
      assert.equal(fixture.requests.length, beforeBlocked, 'F3/E4 block precedes physical HTTP');
      assert.equal(blockedFetch.result.is_error, true, 'F3 blocked fetch is a model-visible error');
      assert.match(eventText(blockedFetch.result), /outside the combined domain policy/u, 'F3 policy cause');
      assert.match(eventText(blockedFetch.message), /^web-error:/u, 'F3 model consumes the error and ends');
      pass('F1/F2/F3: allowed, capped, PDF, and pre-I/O blocked WebFetch execute end to end');

      // F4 same allowed host and visible user URL, but only client results may
      // authorize -> source refusal with zero HTTP. F1 above is the matching
      // positive user-source rule through the identical production pipeline.
      const beforeSource = fixture.requests.length;
      const sourceDenied = await runToolTurn(client, 'web-fetch-source-deny',
        `web-fetch ${fixture.baseURL}/source-denied`, 'web_fetch');
      assert.equal(sourceDenied.result.is_error, true, 'F4 source refusal is durable');
      assert.match(eventText(sourceDenied.result), /url_not_allowed/u, 'F4 source cause');
      assert.equal(fixture.requests.length, beforeSource, 'F4 zero physical HTTP');

      // Search cause/effect graph:
      // C5 selected paid provider is in the process registry; C6 the described
      // credential freezes exact Web-provider audience, header usage, active
      // workspace, and revision; C7 full approximate location exists; C8
      // provider returns allow+block URLs. Effects E5 provider accepts
      // secret+location, E6 production post-filter retains only allow, E7 one
      // linked durable tool lifecycle, E8 plaintext never appears.
      // Constraints K2 one registry clone feeds Control+Host, K4 WebSearchPlugin is
      // the Brain owner, K5 SDK owns Session/events, K6 fixture makes no live search.
      // Decision S1: C5+C6+C7+C8=>E5+E6+E7+E8.
      const search = await runToolTurn(
        client,
        'web-policy-block-search',
        'web-search managed agents',
        'web_search',
      );
      const searchResult = eventText(search.result);
      const searchReply = eventText(search.message);
      assert.equal(
        search.result.is_error,
        false,
        `S1 paid provider executed successfully: ${searchResult}`,
      );
      for (const text of [searchResult, searchReply]) {
        assert.match(text, /https:\/\/allowed\.test\/result/u, 'S1/E6 allowed result retained');
        assert.match(
          text,
          /credential-ok;location=Shanghai\|CN\|Shanghai\|Asia\/Shanghai/u,
          'S1/E5 exact credential and full location reached provider',
        );
        assert.ok(!text.includes('blocked.test'), 'S1/E6 blocked result removed after provider response');
        assert.ok(!text.includes(SEARCH_SECRET), 'S1/E8 raw secret never reaches durable output');
      }
      assert.ok(!JSON.stringify(search.delta).includes(SEARCH_SECRET), 'S1/E8 event history is secret-free');
      pass('S1: paid WebSearch resolves exact credential/location and filters provider results');
    });
  } finally {
    await fixture.close();
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
