import assert from 'node:assert/strict';
import { execFile, execFileSync, spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { appendFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as pause } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { promisify } from 'node:util';
import Anthropic from '@anthropic-ai/sdk';
import { availablePort, listManagedSessionEvents, waitForValue } from '../harness.mjs';
import { createNativeTestEnvironment, sendRollingEvent } from './distributed_control_driver.ts';

const BETAS = ['managed-agents-2026-04-01'];
const API_KEY = 'e2e-dummy';
const WIRE = { 'x-api-key': API_KEY, 'anthropic-version': '2023-06-01', 'anthropic-beta': BETAS[0] };
const isPreview = (e) => e.type === 'event_start' || e.type === 'event_delta';
const messageText = (e) => e.content.map((block) => block.text ?? '').join('');
const fragments = { paced: 100, broker: 100, timeout: 100, burst: 4096, load: 10 };
const exec = promisify(execFile);

export function tail(values) {
  const sorted = values.filter(Number.isFinite).sort((a, b) => a - b);
  const at = (percentile) => sorted[Math.ceil(sorted.length * percentile) - 1] ?? null;
  return { count: sorted.length, p50: at(0.5), p99: at(0.99), p999: at(0.999) };
}

export function previewOutcome(frames, expected, { completed = true } = {}) {
  const messages = frames.filter((e) => e.type === 'agent.message');
  const starts = frames.filter((e) => e.type === 'event_start');
  const deltas = frames.filter((e) => e.type === 'event_delta');
  const endings = frames.filter((e) => e.type === 'session.status_idle');
  assert.ok(starts.length <= 1, 'a preview cannot start twice');
  const id = starts[0]?.event?.id;
  if (starts.length) assert.equal(starts[0].event.type, 'agent.message');
  if (deltas.length) {
    assert.ok(id, 'a delta requires a preview start');
    assert.ok(frames.indexOf(starts[0]) < frames.indexOf(deltas[0]), 'the preview starts before its deltas');
  }
  for (const delta of deltas) {
    assert.equal(delta.event_id, id);
    assert.equal(delta.delta.type, 'content_delta');
    assert.equal(delta.delta.content.type, 'text');
  }
  const text = deltas.map((delta) => delta.delta.content.text).join('');
  assert.ok(expected.startsWith(text), 'the preview must be a contiguous prefix of the final output');
  if (completed) {
    assert.equal(messages.length, 1, 'one committed assistant message');
    assert.equal(messageText(messages[0]), expected);
    assert.equal(endings.length, 1, 'one terminal event');
    assert.equal(endings[0].stop_reason.type, 'end_turn');
    assert.ok(frames.indexOf(messages[0]) < frames.indexOf(endings[0]), 'the final message precedes the terminal event');
    assert.ok(frames.every((event) => event.type !== 'session.error'), 'the Run completes without a Session error');
    if (id) assert.equal(messages[0].id, id, 'preview and committed fact share an identity');
    assert.ok(frames.slice(frames.indexOf(messages[0]) + 1).every((e) => !isPreview(e)), 'no preview after final');
  }
  return { count: deltas.length, length: text.length };
}

function observationPoint(client, session, { idleCount = 1, delayMs = 0, retryAdmission = false } = {}) {
  const abort = new AbortController();
  const frames = [], receivedAt = [];
  const retries = [];
  const address = `${client.baseURL}/v1/sessions/${session}/events/stream?event_deltas%5B%5D=agent.message`;
  const signal = AbortSignal.any([abort.signal, AbortSignal.timeout(120_000)]);
  const opened = (async () => {
    for (let attempt = 0; ; attempt += 1) {
      const response = await fetch(address, { headers: WIRE, signal });
      if (response.status === 200) return response;
      const body = (await response.text()).slice(0, 512);
      if (!retryAdmission || response.status !== 503 || attempt === 3) {
        throw new Error(`Managed Session SSE for ${session} returned ${response.status}: ${body}`);
      }
      retries.push({ status: response.status, body });
      await pause(100 * (attempt + 1), undefined, { signal });
    }
  })();
  const done = opened.then(async (response) => {
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let idle = 0;
    try {
      for (;;) {
        if (delayMs) await pause(delayMs);
        const { value, done: eof } = await reader.read();
        if (eof) break;
        buffer += decoder.decode(value, { stream: true });
        let end;
        while ((end = buffer.indexOf('\n\n')) >= 0) {
          const payload = buffer.slice(0, end).split('\n').filter((line) => line.startsWith('data:')).map((line) => line.slice(5).trimStart());
          buffer = buffer.slice(end + 2);
          if (payload.length === 0) continue;
          const event = JSON.parse(payload.join('\n'));
          frames.push(event);
          receivedAt.push(performance.now());
          if (event.type === 'session.status_idle' && ++idle === idleCount) return;
        }
      }
    } catch (error) {
      if (!abort.signal.aborted) throw error;
    } finally {
      await reader.cancel().catch(() => {});
    }
  });
  done.catch(() => {});
  return { frames, receivedAt, retries, opened, done, abort: () => abort.abort() };
}

function verifyHistory(listed, frames, receipt) {
  assert.ok(listed.every((e) => !isPreview(e)), 'temporary previews stay out of API history');
  assert.equal(listed.filter((e) => e.id === receipt).length, 1, 'exact POST receipt in history');
  const terminalFacts = (events) => events.filter((e) => e.type === 'agent.message' || e.type === 'session.status_idle');
  assert.deepEqual(terminalFacts(frames), terminalFacts(listed), 'stable fact IDs, payloads and order across SSE and history');
}

async function checkHistory(client, session, frames, receipt) {
  verifyHistory(await listManagedSessionEvents(client, session, BETAS), frames, receipt);
}

async function main() {
  const [namespace, baseURL, mode, variant, round] = process.argv.slice(2);
  assert.match(namespace, /^[a-z0-9][a-z0-9-]*$/u);
  const k = (...args) => execFileSync('kubectl', ['-n', namespace, '--request-timeout=10s', ...args], { encoding: 'utf8', timeout: 20_000 });
  const forwards = [];
  const streams = new Set();
  const evidence = process.env.ADR71_PREVIEW_EVIDENCE_DIR;
  assert.ok(evidence && process.env.ADR71_AGENT_ID);
  const report = (row) => {
    appendFileSync(join(evidence, 'results.ndjson'), `${JSON.stringify(row)}\n`);
    const { samples, ...summary } = row;
    console.log(JSON.stringify(summary));
  };
  const clientFor = (url) => new Anthropic({ apiKey: API_KEY, baseURL: url, maxRetries: 0, timeout: 90_000 });
  const open = (client, id, options) => {
    const point = observationPoint(client, id, options);
    streams.add(point);
    point.done.finally(() => streams.delete(point)).catch(() => {});
    return point;
  };
  const forward = async (point) => {
    const port = await availablePort();
    let error = '';
    const child = spawn('kubectl', ['-n', namespace, 'port-forward', point, `${port}:3000`], { stdio: ['ignore', 'pipe', 'pipe'] });
    child.stderr.on('data', (data) => { error = (error + data).slice(-4096); });
    child.stdout.resume();
    forwards.push(child);
    const url = `http://127.0.0.1:${port}`;
    await waitForValue(async () => {
      assert.equal(child.exitCode, null, `port-forward for ${point}: ${error}`);
      return fetch(`${url}/readyz`, { signal: AbortSignal.timeout(1000) }).then((res) => res.ok, () => false);
    }, Boolean, `ready port-forward for ${point}`, { timeoutMs: 20_000 });
    return url;
  };
  const defineRun = (kind) => {
    const marker = `ADR71-PREVIEW-${kind}-${randomUUID()}`;
    return { marker, text: `ADR71-PROVIDER:${marker}` + Array.from({ length: fragments[kind] }, (_, i) => `[${String(i).padStart(4, '0')}]`).join('') };
  };
  const api = clientFor(baseURL);
  const create = (environment_id) => api.beta.sessions.create({ agent: process.env.ADR71_AGENT_ID, environment_id, betas: BETAS });
  const environment = () => createNativeTestEnvironment(baseURL, api, `adr71-preview-${randomUUID()}`);
  const send = (id, marker) => api.beta.sessions.events.send(id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: marker }] }], betas: BETAS,
  }, { headers: { 'Idempotency-Key': marker } });
  const firstPreview = (watch) => waitForValue(() => watch.frames.some((e) => e.type === 'event_delta'), Boolean, 'an actual preview arrived before injecting a fault', { timeoutMs: 15_000 });
  const scaleBroker = (replicas) => exec('kubectl', ['-n', namespace, 'scale', 'deploy/session-preview-nats', `--replicas=${replicas}`], { timeout: 15_000 });

  try {
    const pods = JSON.parse(k('get', 'pods', '-l', 'app=coordinator', '-o', 'json')).items
      .filter((pod) => !pod.metadata.deletionTimestamp && pod.status.conditions?.some((c) => c.type === 'Ready' && c.status === 'True'))
      .sort((a, b) => a.metadata.name.localeCompare(b.metadata.name));
    assert.equal(pods.length, 2, 'two ready Coordinator pods');
    const [a, b] = pods.map((pod) => pod.metadata.name);
    const service = JSON.parse(k('get', 'svc/coordinator', '-o', 'json'));
    const needsNewConnections = service.spec.selector['preview-ingress'] !== 'true'
      || pods[0].metadata.labels['preview-ingress'] !== 'true';
    k('label', `pod/${b}`, 'preview-ingress=false', '--overwrite');
    k('label', `pod/${a}`, 'preview-ingress=true', '--overwrite');
    k('patch', 'svc/coordinator', '--type=merge', '-p', '{"spec":{"selector":{"preview-ingress":"true"}}}');
    await waitForValue(() => JSON.parse(k('get', 'endpoints', 'coordinator', '-o', 'json')).subsets?.flatMap((s) => s.addresses ?? []).map((p) => p.targetRef?.name) ?? [],
      (names) => names.length === 1 && names[0] === a, 'all actual Worker requests route to Coordinator A', { timeoutMs: 20_000 });
    if (needsNewConnections && !(mode === 'route' && variant === 'before-worker-rollout')) {
      // A Service selector does not move established Worker HTTP connections.
      k('rollout', 'restart', 'statefulset/worker');
      execFileSync('kubectl', ['-n', namespace, 'rollout', 'status', 'statefulset/worker', '--timeout=420s'], {
        timeout: 425_000, stdio: 'inherit',
      });
    }
    if (mode === 'route') return;
    const [local, remote] = (await Promise.all([forward(`pod/${a}`), forward(`pod/${b}`)])).map(clientFor);
    const provider = ['broker', 'timeout', 'burst'].includes(mode) ? await forward('deploy/provider') : undefined;
    const release = async (marker, gate) => assert.equal((await fetch(`${provider}/__preview/release/${marker}/${gate}`, {
      method: 'POST', signal: AbortSignal.timeout(5000),
    })).status, 200, `provider gate ${gate} released`);

    async function turn(kind, fault) {
      const { id } = await create(await environment());
      const { marker, text } = defineRun(kind);
      const left = open(local, id), right = open(remote, id);
      await Promise.all([left.opened, right.opened]);
      const accepted = send(id, marker);
      accepted.catch(() => {});
      const replaced = await fault?.({ left, right, id, marker });
      const active = [left, ...(replaced ?? [right])];
      const [receipt] = await Promise.all([accepted, ...active.map((point) => point.done)]);
      const outcomes = active.map((point) => {
        const preview = previewOutcome(point.frames, text);
        return { ...preview, complete: preview.length === text.length };
      });
      for (const [index, point] of active.entries()) {
        await checkHistory(index === 0 ? local : remote, id, point.frames, receipt.data[0].id);
      }
      if (replaced) {
        await right.done;
        previewOutcome(right.frames, text, { completed: false });
      }
      await api.beta.sessions.delete(id, { betas: BETAS });
      return outcomes;
    }

    if (mode === 'cold' || mode === 'mixed') {
      const [left, right] = await turn('paced');
      assert.ok(left.count > 0, 'same-node preview is live');
      if (mode === 'cold') assert.equal(right.count, 0, 'no relay while broker is absent');
      else assert.ok(right.count > 0, 'a real relay carries the mixed-version preview');
      report({ mode, variant, left, right });
    } else if (mode === 'broker') {
      await turn('paced', ({ right }) => firstPreview(right).then(() => undefined));
      let original;
      const [left, reconnectedOnA, reconnectedOnB] = await turn('broker', async ({ right: old, id, marker }) => {
        original = old;
        await firstPreview(old);
        k('scale', 'deploy/session-preview-nats', '--replicas=0');
        await waitForValue(() => JSON.parse(k('get', 'pods', '-l', 'app=session-preview-nats', '-o', 'json')).items.length, (n) => n === 0, 'real preview broker stopped', { timeoutMs: 20_000 });
        old.abort();
        const newNode = open(local, id), sameNode = open(remote, id);
        await Promise.all([newNode.opened, sameNode.opened]);
        await release(marker, 1);
        return [newNode, sameNode];
      });
      assert.equal(reconnectedOnB.count, 0, 'B recovers the committed result while the relay stays down');
      report({ mode: 'broker-outage', originalOnB: original.frames.filter((event) => event.type === 'event_delta').length,
        left, reconnectedOnA, reconnectedOnB });
      const restart = performance.now();
      k('scale', 'deploy/session-preview-nats', '--replicas=1');
      await waitForValue(() => JSON.parse(k('get', 'deployment/session-preview-nats', '-o', 'json')).status.readyReplicas, (ready) => ready === 1, 'preview broker restarted', { timeoutMs: 20_000 });
      // A TCP-ready broker can still be accepting client reconnects. Each
      // independent Run verifies durable completion during that interval.
      const attempts = [];
      while (performance.now() - restart < 45_000) {
        const restored = await turn('load');
        attempts.push(restored);
        if (restored[1].count > 0) break;
      }
      assert.ok(attempts.at(-1)?.[1].count > 0, 'cross-Coordinator preview returns within the broker restart window');
      report({ mode: 'broker-restored', elapsedMs: performance.now() - restart, attempts });
    } else if (mode === 'timeout' || mode === 'burst') {
      const delayed = async () => (await (await fetch(`${provider}/__preview/stats`, { signal: AbortSignal.timeout(1500) })).json()).delayed;
      const before = await delayed();
      const windowStart = new Date().toISOString();
      let loss;
      const [left, right] = await turn(mode, async ({ left: watch, marker }) => {
        await firstPreview(watch);
        k('exec', `pod/${a}`, '-c', 'worker-tls', '--', 'touch', '/tmp/hold-preview');
        try {
          await release(marker, 1);
          await waitForValue(delayed, (count) => count > before, 'a signed HTTP preview batch reached the delay fixture', { timeoutMs: 15_000 });
          const signals = mode === 'timeout' ? ['Worker preview batch timed out'] : [
            'Worker preview queue is full or closed', 'Worker preview batch expired', 'Worker preview batch timed out',
          ];
          loss = await waitForValue(() => {
            const logs = k('logs', '-l', 'app=worker', '-c', 'worker', '--since-time', windowStart, '--tail=5000');
            return signals.find((signal) => logs.includes(signal));
          }, Boolean, 'a Worker dropped a real preview batch during the delay', { timeoutMs: 15_000 });
        } finally {
          k('exec', `pod/${a}`, '-c', 'worker-tls', '--', 'rm', '-f', '/tmp/hold-preview');
        }
        const resumedAt = new Date().toISOString();
        await release(marker, 2);
        await waitForValue(() => /POST \/v1\/worker\/dispatch\/stream_batch HTTP\/[\d.]+ 200/u.test(
          k('logs', `pod/${a}`, '-c', 'worker-tls', '--since-time', resumedAt),
        ), Boolean, 'the Coordinator accepted a later batch after the actual gap', { timeoutMs: 15_000 });
      });
      assert.ok(left.count > 0 && !left.complete, 'partial but coherent preview');
      report({ mode, left, right, loss, delayedBatches: (await delayed()) - before });
    } else if (mode === 'long') {
      const turns = 48;
      const { id } = await create(await environment());
      let lastReceipt;
      for (let index = 1; index <= turns; index += 1) {
        const { marker, text } = defineRun('load');
        lastReceipt = (await send(id, marker)).data[0].id;
        await waitForValue(() => listManagedSessionEvents(local, id, BETAS), (events) =>
          events.filter((event) => event.type === 'session.status_idle').length === index
          && events.some((event) => event.type === 'agent.message' && messageText(event) === text),
        `durable turn ${index}`, { timeoutMs: 60_000 });
      }
      const history = await listManagedSessionEvents(local, id, BETAS);
      assert.ok(history.length > 128, 'replay crosses a delivery page');
      const connected = await Promise.all(Array.from({ length: 16 }, async (_, index) => {
        const point = open(index % 2 ? remote : local, id, { idleCount: turns, delayMs: index % 4 === 0 ? 100 : 0 });
        await point.opened;
        return point;
      }));
      for (const point of connected.slice(0, 8)) point.abort();
      const reconnected = connected.slice(0, 8).map((_, index) => open(index % 2 ? local : remote, id, { idleCount: turns, delayMs: 100 }));
      const checked = [...connected.slice(8), ...reconnected];
      await Promise.all(checked.map(async (point) => {
        await point.done;
        assert.equal(point.frames.filter((e) => e.type === 'session.status_idle').length, turns);
        assert.ok(point.frames.every((e) => !isPreview(e)), 'completed history never repeats preview output');
        verifyHistory(history, point.frames, lastReceipt);
      }));
      await checkHistory(local, id, checked[0].frames, lastReceipt);
      await api.beta.sessions.delete(id, { betas: BETAS });
      report({ mode, turns, events: history.length, checked: checked.length, reconnected: reconnected.length });
    } else if (mode === 'load' || mode === 'load-fault') {
      const total = Number(process.env.ADR71_PREVIEW_RUNS ?? '1200');
      const concurrency = Number(process.env.ADR71_PREVIEW_CONCURRENCY ?? '32');
      assert.ok(Number.isSafeInteger(total) && total > 0 && Number.isSafeInteger(concurrency) && concurrency > 0 && concurrency <= total);
      const environments = await Promise.all(Array.from({ length: concurrency }, environment));
      const loadStarted = performance.now();
      const samples = [];
      let next = 0, inFlight = 0, atOutage = 0;
      let fault;
      await Promise.all(environments.map(async (environmentId) => {
        while (next < total) {
          const index = next++;
          const { id } = await create(environmentId);
          const { marker, text } = defineRun('load');
          const connecting = performance.now();
          const sub = open(remote, id, { retryAdmission: true });
          await sub.opened;
          const start = performance.now();
          const publicationRetries = [];
          const accepted = sendRollingEvent(api, id, marker, marker, (status) => publicationRetries.push(status));
          inFlight += 1;
          if (mode === 'load-fault' && !fault && next >= Math.ceil(total / 3) && inFlight >= Math.ceil(concurrency / 2)) {
            atOutage = inFlight;
            fault = (async () => {
              await scaleBroker(0);
              await waitForValue(async () => {
                const { stdout } = await exec('kubectl', ['-n', namespace, 'get', 'pods', '-l', 'app=session-preview-nats', '-o', 'json'], { timeout: 15_000 });
                return JSON.parse(stdout).items.length;
              }, (n) => n === 0, 'real NATS outage while load continues', { timeoutMs: 30_000 });
              return samples.length;
            })();
            fault.catch(() => {});
          }
          const [receipt] = await Promise.all([accepted, sub.done]);
          inFlight -= 1;
          let coherent = true;
          try {
            previewOutcome(sub.frames, text);
          } catch (error) {
            // A baseline preview can have a gap. The formal output must still be exact.
            try {
              previewOutcome(sub.frames.filter((event) => !isPreview(event)), text);
            } catch (failure) {
              const summarize = (events) => events.filter((event) => !isPreview(event)).map((event) => ({
                id: event.id, type: event.type, stopReason: event.stop_reason?.type, error: event.error,
              }));
              const history = await listManagedSessionEvents(remote, id, BETAS).then(summarize, (cause) => String(cause));
              report({ mode: 'load-run-failure', variant, round, session: id, receipt: receipt.data[0].id,
                streamed: summarize(sub.frames), history });
              throw failure;
            }
            if (variant !== 'baseline') throw error;
            coherent = false;
          }
          if (mode === 'load-fault' || publicationRetries.length || index % 16 === 0) {
            const history = await waitForValue(() => listManagedSessionEvents(remote, id, BETAS), () => true, 'committed history audit', {
              timeoutMs: 15_000, retryReadError: (error) => error instanceof Anthropic.APIError && error.status === 503,
            });
            verifyHistory(history, sub.frames, receipt.data[0].id);
            const taken = history.filter((event) => event.type === 'user.message' && messageText(event) === marker);
            assert.deepEqual(taken.map((event) => event.id), [receipt.data[0].id], 'retry publishes one user event');
          }
          const first = sub.receivedAt[sub.frames.findIndex((e) => e.type === 'event_delta')];
          const final = sub.receivedAt[sub.frames.findIndex((e) => e.type === 'agent.message')];
          const terminal = sub.receivedAt[sub.frames.findIndex((e) => e.type === 'session.status_idle')];
          samples.push({ coherent, admissionMs: start - connecting, admissionRetries: sub.retries, publicationRetries,
            firstMs: first === undefined ? null : first - start, formalMs: final - start, terminalMs: terminal - start });
        }
      }));
      assert.equal(samples.length, total);
      let completedDuringOutage;
      if (mode === 'load-fault') {
        assert.ok(fault, 'some load lanes must be active when NATS is stopped');
        completedDuringOutage = total - await fault;
        assert.ok(completedDuringOutage > 0, 'Runs must complete after NATS actually stops');
        await scaleBroker(1);
        k('rollout', 'status', 'deploy/session-preview-nats', '--timeout=15s');
      }
      report({ mode, variant, round, concurrency, elapsedMs: performance.now() - loadStarted, atOutage, completedDuringOutage,
        incoherentPreviews: samples.filter((sample) => !sample.coherent).length,
        admissionRetries: samples.reduce((sum, sample) => sum + sample.admissionRetries.length, 0),
        publicationRetries: samples.reduce((sum, sample) => sum + sample.publicationRetries.length, 0),
        admissionMs: tail(samples.map((sample) => sample.admissionMs)),
        firstMs: tail(samples.map((sample) => sample.firstMs)), formalMs: tail(samples.map((sample) => sample.formalMs)),
        terminalMs: tail(samples.map((sample) => sample.terminalMs)), samples });
    } else {
      assert.fail(`unknown preview scenario ${mode}`);
    }
  } catch (error) {
    report({ mode, variant, round, failure: String(error), stack: error.stack });
    throw error;
  } finally {
    for (const stream of streams) stream.abort();
    for (const child of forwards) child.kill();
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
