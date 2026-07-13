// k6 scenario validation for the awaken-server-local management plane (ADR-0043).
//
// It drives the HTTP surfaces directly (not the SDK): the admin config plane
// (`/v1/config/*`), the Managed vault/credential front door (`/v1/vaults/*`), and a
// Managed session turn (`/v1/sessions/*`, echo model). Every step asserts the wire
// shape with `check()`, so this is a functional gate — but the SAME script scales to
// a concurrent load/stress run by flipping `K6_PROFILE=stress`, which only changes
// the executor (VUs/duration), never the assertions.
//
// Run against a server on $BASE_URL (see run.sh, which starts `management` mode):
//   k6 run -e BASE_URL=http://127.0.0.1:38200 e2e/k6/management_scenarios.js
//   k6 run -e BASE_URL=... -e K6_PROFILE=stress e2e/k6/management_scenarios.js

import http from 'k6/http';
import { check, group } from 'k6';

const BASE = __ENV.BASE_URL || 'http://127.0.0.1:38200';
const BETA = { 'anthropic-beta': 'managed-agents-2026-04-01', 'content-type': 'application/json' };
const PROFILE = __ENV.K6_PROFILE || 'smoke';

// Smoke = a few sequential iterations (a correctness gate). Stress = ramping VUs
// hammering the same flows concurrently (throughput/latency/lock-contention).
export const options =
  PROFILE === 'stress'
    ? {
        scenarios: {
          stress: {
            executor: 'ramping-vus',
            startVUs: 0,
            stages: [
              { duration: '10s', target: Number(__ENV.VUS || 30) },
              { duration: '20s', target: Number(__ENV.VUS || 30) },
              { duration: '5s', target: 0 },
            ],
          },
        },
        thresholds: {
          checks: ['rate>0.99'],
          http_req_failed: ['rate<0.01'],
          http_req_duration: ['p(95)<500'],
        },
      }
    : {
        scenarios: {
          smoke: { executor: 'shared-iterations', vus: 1, iterations: Number(__ENV.ITERS || 3) },
        },
        // A smoke run is a gate: every check must pass and nothing may error.
        thresholds: { checks: ['rate==1.0'], http_req_failed: ['rate==0'] },
      };

function post(path, body) {
  return http.post(`${BASE}${path}`, JSON.stringify(body), { headers: BETA });
}
function put(path, body) {
  return http.put(`${BASE}${path}`, JSON.stringify(body), { headers: BETA });
}
function get(path) {
  return http.get(`${BASE}${path}`, { headers: BETA });
}

export default function () {
  // Unique per VU+iteration so concurrent stress runs don't collide.
  const uid = `${__VU}-${__ITER}`;

  group('admin config plane', () => {
    check(put('/v1/config/providers/anthropic', {
      id: 'anthropic', slug: 'anthropic', display_name: 'Anthropic', version: 1,
    }), { 'provider 200': (r) => r.status === 200 });

    check(put('/v1/config/endpoints/ep1', {
      id: 'ep1', provider_id: 'anthropic', flavor: 'anthropic_messages',
      base_url: 'https://api.anthropic.com/v1/', timeout_secs: 300, display_name: 'prod', version: 1,
    }), { 'endpoint 200': (r) => r.status === 200 });

    check(post('/v1/config/offerings', {
      model_id: 'claude-opus-4-8', provider_id: 'anthropic',
      protocol_endpoint_id: 'ep1', flavor: 'anthropic_messages', upstream_model: null,
    }), { 'offering 200': (r) => r.status === 200 });

    check(get('/v1/config/catalog'), {
      'catalog 200': (r) => r.status === 200,
      'catalog has provider': (r) => r.json('providers.anthropic') !== undefined,
    });

    const cred = post('/v1/config/credentials', {
      workspace_id: `ws-${uid}`, kind: 'vault', provider_id: 'anthropic',
      env_key: 'ANTHROPIC_API_KEY', secret: `sk-k6-${uid}`,
    });
    check(cred, {
      'credential 201': (r) => r.status === 201,
      'credential secret-free': (r) => !r.body.includes('sk-k6-'),
    });
    const credId = cred.json('id');

    check(post('/v1/config/inference/resolve', {
      workspace_id: `ws-${uid}`, model_id: 'claude-opus-4-8',
      binding: { type: 'exact', credential_source_id: credId },
    }), {
      'resolve 200': (r) => r.status === 200,
      'resolve adapter=anthropic': (r) => r.json('adapter_kind') === 'anthropic',
      'resolve credential_present': (r) => r.json('credential_present') === true,
    });
  });

  group('managed vault front door', () => {
    const vault = post('/v1/vaults', { display_name: `k6-${uid}` });
    check(vault, { 'vault 200': (r) => r.status === 200, 'vault type': (r) => r.json('type') === 'vault' });
    const vaultId = vault.json('id');

    const cred = post(`/v1/vaults/${vaultId}/credentials`, {
      type: 'environment_variable', secret_name: 'ANTHROPIC_API_KEY',
      secret_value: `sk-k6-vault-${uid}`, networking: { type: 'unrestricted' },
    });
    check(cred, {
      'vault credential 200': (r) => r.status === 200,
      'vault credential type': (r) => r.json('type') === 'vault_credential',
      'vault credential secret-free': (r) => !r.body.includes('sk-k6-vault-'),
    });
    const credId = cred.json('id');

    check(post(`/v1/vaults/${vaultId}/credentials/${credId}/mcp_oauth_validate`, {}), {
      'validate 200': (r) => r.status === 200,
      'validate unknown': (r) => r.json('status') === 'unknown',
    });
  });

  group('managed session turn (echo)', () => {
    const session = post('/v1/sessions', { agent: 'assistant', environment_id: 'env_local' });
    check(session, {
      'session 200': (r) => r.status === 200,
      'session idle': (r) => r.json('status') === 'idle',
    });
    const sid = session.json('id');

    check(post(`/v1/sessions/${sid}/events`, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: `hello ${uid}` }] }],
    }), { 'send events 200': (r) => r.status === 200 });

    const events = get(`/v1/sessions/${sid}/events`);
    check(events, {
      'events 200': (r) => r.status === 200,
      'echoed the turn': (r) => {
        const data = r.json('data') || [];
        const msg = data.find((e) => e.type === 'agent.message');
        return msg && msg.content[0].text === `Echo: hello ${uid}`;
      },
    });
  });
}
