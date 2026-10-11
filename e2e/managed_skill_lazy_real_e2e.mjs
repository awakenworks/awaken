// Native Skill startup qualification through production Awaken, real DeepSeek,
// the official SDK and independent Kubernetes observations. No fake model.
// L1 cold/warm Session + pinned reads -> exact unknown reference bytes, reply
// within the configured budget, zero Session-scoped Sandbox Pods.
// L2 service container restart + same Session -> a new exact read and reply,
// same retained pin, no Sandbox. L3 first Bash -> real script output + Ready Pod.
// L4 unschedulable CPU request -> L1 still succeeds; Bash later fails, without
// successful script result. Missing infrastructure/key/observations fail, never
// become a successful skip. Every receipt fences history to its own turn.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import Anthropic from '@anthropic-ai/sdk';
import { DefaultChatTransport, lastAssistantMessageIsCompleteWithToolCalls } from 'ai';
import { Chat } from '@ai-sdk/react';
import {
  SKILLS_BETA, bindSandboxExecutionPolicy, createCrossProtocolApplicationThread, managedFileUploadForm,
  provisionProviderTestModel, waitForPort, waitForSessionEventReceipt, waitForValue,
} from './harness.mjs';

const execute = promisify(execFile);
assert.equal(process.argv.length, 2, 'test configuration uses environment variables only');
const baseURL = process.env.AWAKEN_LAZY_TEST_URL;
const context = process.env.AWAKEN_LAZY_KUBE_CONTEXT;
const namespace = process.env.AWAKEN_LAZY_KUBE_NAMESPACE ?? 'awaken-system';
const serviceContainer = process.env.AWAKEN_LAZY_SERVICE_CONTAINER;
const output = path.resolve(process.env.AWAKEN_EVAL_ARTIFACT_DIR ?? 'target/test-evidence/skill-lazy-live');
const replyBudget = Number(process.env.AWAKEN_LAZY_REPLY_BUDGET_MS ?? 30_000);
const startupSamples = Number(process.env.AWAKEN_LAZY_STARTUP_SAMPLES ?? 0);
const imagePrewarm = process.env.AWAKEN_LAZY_IMAGE_PREWARM === '1';
let directoryInit = process.env.AWAKEN_LAZY_DIRECTORY_INIT === '1';
const upgradeImage = process.env.AWAKEN_LAZY_UPGRADE_RUNTIME_IMAGE;
assert.ok(Number.isSafeInteger(startupSamples) && startupSamples >= 0 && startupSamples <= 20, 'startup samples 0..20');
const key = process.env.DEEPSEEK_API_KEY || (process.env.DEEPSEEK_API_KEY_FILE
  ? fs.readFileSync(process.env.DEEPSEEK_API_KEY_FILE, 'utf8').trim() : null);
assert.ok(baseURL && context && serviceContainer?.startsWith('awaken-skill-lazy-') && key, 'explicit owned service, Kubernetes context and DeepSeek key required');
assert.ok(Number.isSafeInteger(replyBudget) && replyBudget > 0);
const betas = ['managed-agents-2026-04-01'];
const client = new Anthropic({ baseURL, apiKey: 'e2e-local', maxRetries: 0 }); // awaken-allow(secret): (no-login test deployment)
const tag = randomBytes(6).toString('hex');
const referenceMarker = `REFERENCE_${tag}`;
const executionMarker = `EXECUTION_${tag}`;
const recoveryMarker = `RECOVERY_${tag}`;
const evidence = { model_source: 'live-deepseek', context, namespace, service_container: serviceContainer,
  runtime_placement: 'production-cli-and-worker-in-host-network-container', sandbox_placement: 'k3d-pod',
  source_commit: process.env.AWAKEN_LAZY_SOURCE_COMMIT, reply_budget_ms: replyBudget,
  image_prewarm: imagePrewarm, startup_samples: startupSamples, observations: [], passed: false };
evidence.storage_profile = process.env.AWAKEN_LAZY_STORAGE_PROFILE ?? 'local-path';
evidence.runtime_source_commit = process.env.AWAKEN_LAZY_RUNTIME_SOURCE_COMMIT;
const owned = { sessions: [], environments: [], agent: null, skill: null };
function record(observation) {
  evidence.observations.push(observation);
  fs.writeFileSync(path.join(output, 'evidence.json'), JSON.stringify(evidence, null, 2));
}

async function kube(...args) {
  const { stdout } = await execute('kubectl', ['--context', context, '-n', namespace, ...args], { timeout: 15_000, maxBuffer: 8 * 1024 * 1024 });
  return stdout;
}
async function scopedPods(sessionId) {
  const { items } = JSON.parse(await kube('get', 'pods', '-o', 'json'));
  return items.filter((pod) => pod.metadata.annotations?.['awaken.dev/sandbox-scope'] === sessionId)
    .map((pod) => ({ name: pod.metadata.name, uid: pod.metadata.uid, phase: pod.status?.phase,
      ready: pod.status?.conditions?.some((condition) => condition.type === 'Ready' && condition.status === 'True') ?? false,
      scheduled: pod.status?.conditions?.find((condition) => condition.type === 'PodScheduled'),
      created_at: pod.metadata.creationTimestamp, observed_at: new Date().toISOString(),
      conditions: pod.status?.conditions, containers: pod.status?.containerStatuses, init_containers: pod.status?.initContainerStatuses,
      resources: pod.spec?.containers?.map((container) => ({ name: container.name, resources: container.resources })), volumes: pod.spec?.volumes }));
}
async function send(sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }], betas,
  });
  assert.ok(receipt.data[0]?.id, 'exact input receipt');
  return receipt.data[0].id;
}
function resultText(events, name) {
  const ids = new Set(events.filter((event) => event.type === 'agent.tool_use' && event.name === name).map((event) => event.id));
  return events.filter((event) => event.type === 'agent.tool_result' && ids.has(event.tool_use_id))
    .map((event) => JSON.stringify(event.content)).join('\n');
}
async function readTurn(sessionId, label, instructionsPreloaded = false) {
  const start = performance.now();
  let firstTextMs;
  // Restored conversation already knows facts.md. A distinct, undisclosed
  // bundled reference makes a new read necessary without forcing redundant
  // reloads of information that legitimately survives in conversation history.
  const restored = label === 'cold-restored-session' || label === 'preloaded-warm-session';
  const expected = restored ? recoveryMarker : referenceMarker;
  const id = await send(sessionId, restored
    ? '请读取这个 Skill 中尚未读过的 references/recovery.md。根据它给我一个简短问题，并带上其中的识别标记。'
    : instructionsPreloaded
      ? '我是做家具的，想招经销商。请按已提供的方法，读取它要求的参考资料，给我一个自然的简短问题，并带上资料中的识别标记。'
      : '我是做家具的，想招经销商。请先读取已绑定 Skill 以及它要求的参考资料，给我一个自然的简短问题，并带上资料中的识别标记。');
  const { delta } = await waitForSessionEventReceipt(client, sessionId, id, betas,
    ({ delta }) => {
      if (firstTextMs === undefined && delta.some((event) => event.type === 'agent.message'
          && event.content?.some((block) => block.type === 'text' && block.text?.trim()))) {
        firstTextMs = performance.now() - start;
      }
      return firstTextMs !== undefined && delta.some((event) => event.type === 'session.status_idle' && event.stop_reason?.type === 'end_turn');
    },
    `${label}: complete new reply`, { timeoutMs: 90_000, pollMs: 100 });
  const elapsed = performance.now() - start;
  const pods = await scopedPods(sessionId);
  record({ label, session_id: sessionId, input_receipt: id,
    first_text_ms: firstTextMs, complete_reply_ms: elapsed, pods, events: delta });
  assert.ok(delta.some((event) => event.type === 'agent.tool_use' && event.name === 'read'), 'live model chose read');
  assert.ok(resultText(delta, 'read').includes(expected), 'new tool result carries unknown reference bytes');
  // R1 successful unknown reference bytes -> subsequent model inference and
  // a nonempty reply, even when natural prose does not echo the test marker.
  // R2 missing bytes is rejected above; R3 only a pre-read announcement or no
  // subsequent inference/reply is rejected here. This qualifies delivery into
  // the model context, not the semantic quality of the model's prose.
  const referenceResultIndex = delta.findIndex((event) => event.type === 'agent.tool_result'
    && !event.is_error && JSON.stringify(event.content).includes(expected));
  const inferenceIndex = delta.findIndex((event, index) => index > referenceResultIndex
    && event.type === 'span.model_request_start');
  assert.ok(referenceResultIndex >= 0 && inferenceIndex > referenceResultIndex, 'model inference follows the successful reference read');
  assert.ok(delta.some((event, index) => index > inferenceIndex && event.type === 'agent.message'
    && event.content?.some((block) => block.type === 'text' && block.text?.trim())), 'model replies after receiving reference bytes');
  assert.ok(!delta.some((event) => event.type === 'agent.tool_use' && event.name !== 'read'), 'no shell probe before reply');
  if (instructionsPreloaded) assert.ok(!delta.some(event => event.type === 'agent.tool_use'
    && event.name === 'read' && JSON.stringify(event.input).includes('/SKILL.md')),
  'full system instructions do not require another Skill body read; unknown reference still read');
  assert.equal(pods.length, 0, 'read/reply does not create Sandbox Pod');
  assert.ok(firstTextMs <= replyBudget, `${label} first text ${firstTextMs.toFixed(0)}ms exceeds ${replyBudget}ms`);
  console.log(`${label}: first text ${firstTextMs.toFixed(0)}ms; complete ${elapsed.toFixed(0)}ms; zero Sandbox Pods`);
}

async function clientQuestionTurn(environmentId, agent, instructions) {
  // X1 one pinned Skill + a standard custom frontend tool; X2 the official
  // AI SDK sends customer text and supplies the tool answer. X1+X2 -> one
  // byte-faithful user.message, one correlated user.custom_tool_result, then
  // real model inference/reply; no Agent-tool result for the same occurrence
  // and no Sandbox. This is transport/context delivery, not a semantic score.
  const id = await session(environmentId, { type: 'agent_with_overrides', id: agent.id,
    version: agent.version, system: 'The complete startup-diagnosis Skill instructions follow.\n'
      + instructions + '\nAfter reading its reference, call AskUserQuestion once to ask whether any partner shop has opened. After receiving the frontend answer, reply briefly and do not call another tool.',
    tools: [...agent.tools, { type: 'custom', name: 'AskUserQuestion',
      description: 'Ask the customer one question, then await the frontend answer.',
      input_schema: { type: 'object', properties: { question: { type: 'string' } }, required: ['question'] } }] });
  const { headers } = await createCrossProtocolApplicationThread(baseURL, agent.id, id);
  const chat = new Chat({ id, transport: new DefaultChatTransport({
    api: `${baseURL}/v1/ai-sdk/threads/${id}/runs`, headers,
    fetch: (input, init) => fetch(input, { ...init,
      signal: AbortSignal.any([...(init?.signal ? [init.signal] : []), AbortSignal.timeout(90_000)]) }),
  }), sendAutomaticallyWhen: lastAssistantMessageIsCompleteWithToolCalls });
  const customer = '我是做家具的，想招经销商。服务费金额本次未提供，收款时间还没有定。';
  const start = performance.now();
  await chat.sendMessage({ text: customer });
  assert.equal(chat.error, undefined, 'official client receives the question');
  const part = chat.lastMessage?.parts.find(part => part.type === 'tool-AskUserQuestion');
  assert.ok(part?.toolCallId && part.state === 'input-available', 'real model calls frontend question');
  const initial = [];
  for await (const event of client.beta.sessions.events.list(id, { order: 'asc', limit: 100, betas })) initial.push(event);
  const call = initial.find(event => event.type === 'agent.custom_tool_use' && event.name === 'AskUserQuestion');
  assert.ok(call, 'standard Managed call matches the frontend tool');
  const answer = { answer: '合作门店还没有开业' };
  await chat.addToolOutput({ tool: 'AskUserQuestion', toolCallId: part.toolCallId, output: answer });
  const answerReceipt = await waitForValue(async () => {
    const rows = [];
    for await (const event of client.beta.sessions.events.list(id, { order: 'asc', limit: 100, betas })) rows.push(event);
    return rows.find(event => event.type === 'user.custom_tool_result'
      && event.custom_tool_use_id === call.id && JSON.stringify(event.content) === JSON.stringify([{ type: 'text', text: JSON.stringify(answer) }]));
  }, receipt => Boolean(receipt?.processed_at), 'exact frontend answer commit', { timeoutMs: 90_000, pollMs: 100 });
  const { events } = await waitForSessionEventReceipt(client, id, answerReceipt.id, betas,
  ({ delta }) => {
    const modelIndex = delta.findIndex(event => event.type === 'span.model_request_start');
    return modelIndex >= 0
      && delta.some((event, index) => index > modelIndex && event.type === 'agent.message'
        && event.content?.some(block => block.type === 'text' && block.text?.trim()))
      && delta.some(event => event.type === 'session.status_idle' && event.stop_reason?.type === 'end_turn');
  },
  'AI SDK answer reaches the real model and the Session ends', { timeoutMs: 90_000, pollMs: 100 });
  const pods = await scopedPods(id);
  record({ label: 'ai-sdk-frontend-question', session_id: id, elapsed_ms: performance.now() - start, pods, events });
  const users = events.filter(event => event.type === 'user.message');
  assert.equal(users.length, 1, 'one actual customer input in Managed history');
  assert.deepEqual(users[0].content, [{ type: 'text', text: customer }], 'unprovided and unset remain distinct bytes');
  const replies = events.filter(event => event.type === 'user.custom_tool_result' && event.custom_tool_use_id === call.id);
  assert.equal(replies.length, 1, 'one exact public answer occurrence');
  assert.deepEqual(replies[0].content, [{ type: 'text', text: JSON.stringify(answer) }]);
  assert.ok(!events.some(event => event.type === 'agent.tool_result' && event.tool_use_id === call.id), 'no second result family');
  const replyIndex = events.indexOf(replies[0]);
  assert.ok(events.some((event, index) => index > replyIndex && event.type === 'span.model_request_start'), 'model inference receives the answer');
  assert.equal(pods.length, 0, 'frontend question and answer require no Sandbox');
  console.log(`ai-sdk-frontend-question: ${Math.round(performance.now() - start)}ms; exact input/answer history; zero Sandbox Pods`);
}
async function environment(failure = false) {
  const env = await client.beta.environments.create({ name: `skill-lazy-${tag}-${failure ? 'failure' : 'normal'}`, config: { type: 'self_hosted' }, betas });
  owned.environments.push(env.id);
  await bindSandboxExecutionPolicy(baseURL, env.id, { id: `skill-lazy-${tag}-${failure ? 'failure' : 'normal'}`, provisioning: 'on_tool_use',
    config: failure ? { requests: { cpu_millis: 1_000_000 } } : {} });
  return env.id;
}
async function session(environmentId, selectedAgent = owned.agent) {
  const start = performance.now();
  const value = await client.beta.sessions.create({ agent: selectedAgent, environment_id: environmentId, betas });
  if (typeof selectedAgent === 'object' && Object.hasOwn(selectedAgent, 'system')) {
    // P0 a standard Session override replaces Agent.system exactly, then the
    // persistent read returns that same full method. This distinguishes lost
    // delivery from the model choosing a redundant read under valid guidance.
    assert.equal(value.agent.system, selectedAgent.system, 'create echoes exact system override');
    const restored = await client.beta.sessions.retrieve(value.id, { betas });
    assert.equal(restored.agent.system, selectedAgent.system, 'persistent Session preserves exact system override');
  }
  owned.sessions.push(value.id);
  record({ label: 'session-create', session_id: value.id, duration_ms: performance.now() - start });
  return value.id;
}
async function bashTurn(sessionId, failure = false) {
  const metrics = async () => {
    if (!startupSamples) return null;
    const url = new URL(baseURL); url.port = String(Number(url.port) + 2); url.pathname = '/metrics';
    const response = await fetch(url); assert.ok(response.ok, 'real runtime metrics required');
    return (await response.text()).split('\n').filter((line) => /awaken_hand_lifecycle|awaken_tool_execution|gen_ai_client/.test(line)).join('\n');
  };
  const beforeMetrics = await metrics();

  const start = performance.now();
  const id = await send(sessionId, `请实际执行一次脚本，不复用历史结果。调用 bash 工具执行命令 bash /workspace/.skills/${owned.skill}/scripts/verify.sh，不切换工作目录。成功时告诉我这次实际输出；工具失败时停止重试。`);
  const seen = new Map();
  const seenHelpers = new Map();
  let monitorFailure;
  let settledReceipt = null;
  const monitor = waitForValue(async () => {
      try {
        for (const pod of await scopedPods(sessionId)) seen.set(`${pod.uid}:${pod.phase}:${pod.ready}:${JSON.stringify(pod.containers?.map((container) => container.state))}`, pod);
        if (startupSamples) {
          for (const pod of JSON.parse(await kube('get', 'pods', '-n', 'kube-system', '-o', 'json')).items.filter((pod) => pod.metadata.name.startsWith('helper-pod-'))) {
            seenHelpers.set(`${pod.metadata.uid}:${JSON.stringify(pod.status?.containerStatuses)}`, { name: pod.metadata.name, uid: pod.metadata.uid, created_at: pod.metadata.creationTimestamp, observed_at: new Date().toISOString(), status: pod.status });
          }
        }
      } catch (error) {
        // Read-only transport observations may be retried. Preserve the exact
        // failure; never manufacture an empty successful Pod observation.
        record({ label: 'pod-observation-error', session_id: sessionId, error: String(error) });
        if (!/TLS handshake timeout|Unable to connect to the server|i\/o timeout|connection reset/iu.test(String(error))) {
          monitorFailure = error;
          return id;
        }
      }
      return settledReceipt;
  }, (receipt) => receipt === id, 'exact input receipt settled while collecting Pod observations',
  { timeoutMs: 230_000, pollMs: startupSamples ? 250 : 1000 });
  let delta;
  try {
    ({ delta } = await waitForSessionEventReceipt(client, sessionId, id, betas,
      ({ delta }) => delta.some((event) => event.type === 'session.status_idle')
        && delta.some((event) => event.type === 'agent.tool_result' || event.type === 'session.error'),
      'first physical tool settles', { timeoutMs: 210_000, pollMs: 100 }));
  } finally { settledReceipt = id; await monitor; }
  // O1 the monitor misses the Ready edge during a transient TLS outage but the
  // final independent read sees it => retain that real snapshot. O2 final and
  // monitored reads both show no Ready => still reject. Never infer Pod readiness
  // from the model's reply. Reuse this final read in detailed evidence below.
  const finalPods = await scopedPods(sessionId);
  const pods = [...seen.values(), ...finalPods];
  record({ label: failure ? 'unschedulable-startup' : 'first-bash', session_id: sessionId,
    input_receipt: id, elapsed_ms: performance.now() - start, pods, events: delta });
  if (monitorFailure) throw monitorFailure;
  if (startupSamples) {
    // Cause/effect sampling rules T1 cold node; T2 cached image/new Session;
    // T3 same Session repeat. Collect real clocks, exact Pod/PVC UID events,
    // Hand/tool/model metric deltas and actual script stdout; never infer a
    // successful execution from assistant prose or rounded history timestamps.
    const currentPods = JSON.parse(await kube('get', 'pods', '-o', 'json')).items.filter((pod) => pods.some((observed) => observed.uid === pod.metadata.uid));
    const claimNames = new Set(currentPods.flatMap((pod) => pod.spec.volumes ?? []).map((volume) => volume.persistentVolumeClaim?.claimName).filter(Boolean));
    const claims = JSON.parse(await kube('get', 'pvc', '-o', 'json')).items.filter((claim) => claimNames.has(claim.metadata.name));
    const uids = new Set([...pods.map((pod) => pod.uid), ...claims.map((claim) => claim.metadata.uid)]);
    const events = JSON.parse(await kube('get', 'events', '-o', 'json')).items.filter((event) => uids.has(event.involvedObject?.uid));
    const helpers = JSON.parse(await kube('get', 'pods', '-n', 'kube-system', '-o', 'json')).items.filter((pod) => pod.metadata.name.startsWith('helper-pod-'));
    const logs = [];
    for (const pod of currentPods) logs.push({ pod: pod.metadata.name, text: await kube('logs', pod.metadata.name, '-c', 'agent', '--timestamps', '--tail=100') });
    record({ label: 'startup-details', session_id: sessionId, metrics_before: beforeMetrics, metrics_after: await metrics(), events,
      pods: finalPods, claims: claims.map((claim) => ({ uid: claim.metadata.uid, created_at: claim.metadata.creationTimestamp, status: claim.status })),
      helper_pods: [...seenHelpers.values(), ...helpers.map((pod) => ({ name: pod.metadata.name, created_at: pod.metadata.creationTimestamp, status: pod.status }))], logs });
    if (imagePrewarm) assert.ok(!events.some((event) => event.reason === 'Pulled' && /Successfully pulled image/.test(event.message)), 'T2 prewarmed node must not download Sandbox image in first tool');
  }

  assert.ok(delta.some((event) => event.type === 'agent.tool_use' && event.name === 'bash'), 'live model requested Bash');
  if (failure) {
    assert.ok(pods.some((pod) => pod.scheduled?.status === 'False' && pod.scheduled?.reason === 'Unschedulable'), 'independent scheduler failure evidence');
    assert.ok(!pods.some((pod) => pod.ready), 'failed Pod never became Ready');
    assert.ok(!resultText(delta, 'bash').includes(executionMarker), 'no successful script output after startup failure');
    assert.ok(delta.some((event) => event.type === 'session.error' || (event.type === 'agent.tool_result' && event.is_error)), 'failure reaches public history');
  } else {
    assert.ok(pods.some((pod) => pod.ready), 'first physical tool creates a real Ready Pod');
    assert.equal(pods.some((pod) => pod.init_containers?.some((item) => item.name === 'continuation-init')), directoryInit,
      'only historical Pods contain directory init; fresh Pods use kubelet subPaths');
    assert.ok(resultText(delta, 'bash').includes(executionMarker), 'actual script output committed');
  }
}

async function main() {
  fs.mkdirSync(output, { recursive: true, mode: 0o700 });
  await waitForPort(Number(new URL(baseURL).port), 15_000);
  const workspaceResponse = await fetch(`${baseURL}/v1/config/workspace-context`);
  assert.ok(workspaceResponse.ok);
  const workspace = workspaceResponse.headers.get('anthropic-workspace-id');
  assert.ok(workspace);
  const supplied = await provisionProviderTestModel(baseURL, workspace, {
    id: `skill-lazy-${tag}`, provider_id: 'deepseek', dialect: 'open_ai_chat', base_url: 'https://api.deepseek.com',
    secret: key, model_id: process.env.DEEPSEEK_MODEL ?? 'deepseek-flash', timeout_secs: 90,
  });
  const instructions = 'Before answering, read references/facts.md. Include its reference marker with one brief Chinese question.\nWhen explicitly asked to execute, use bash to run scripts/verify.sh from this Skill directory. Never retry a failed tool.\n';
  const form = managedFileUploadForm('---\nname: startup-diagnosis\ndescription: 家具招商诊断，先阅读参考资料再提问\n---\n' + instructions, 'SKILL.md');
  form.append('file', new Blob([`${referenceMarker}\n产品是家具，客户计划招经销商。\n`]), 'references/facts.md');
  form.append('file', new Blob([`${recoveryMarker}\n现在需要澄清合作门店是否已经开业。\n`]), 'references/recovery.md');
  form.append('file', new Blob([`#!/bin/sh\nset -eu\nprintf 'SCRIPT_BEGIN '; date -u +%s%N\nstate=/workspace/.skill-lazy-state\nif [ ! -e "$state" ]; then printf '%s' '${executionMarker}' > "$state"; fi\ntest "$(cat "$state")" = '${executionMarker}'\nprintf '%s\\n' '${executionMarker}'\nprintf 'SCRIPT_END '; date -u +%s%N\n`]), 'scripts/verify.sh');
  const upload = await fetch(`${baseURL}/v1/skills`, { method: 'POST', headers: { 'anthropic-beta': SKILLS_BETA }, body: form });
  assert.ok(upload.ok); const skill = await upload.json(); owned.skill = skill.id;
  const agent = await client.beta.agents.create({ name: `Skill lazy qualification ${tag}`, model: supplied.model,
    system: 'You help a Chinese SME owner. Use the attached Skill before answering. Read its reference anew on each relevant request. Do not disclose internal process narration. Use Bash only when the user explicitly asks to run the script. If any tool fails, stop retrying and explain briefly.',
    skills: [{ type: 'custom', skill_id: skill.id, version: '1' }],
    tools: [{ type: 'agent_toolset_20260401', default_config: { enabled: false, permission_policy: { type: 'always_allow' } }, configs: [{ name: 'read', enabled: true }, { name: 'bash', enabled: true }] }], betas });
  owned.agent = agent.id;
  const normalEnv = await environment();
  if (imagePrewarm) {
    const started = performance.now();
    const holders = await waitForValue(async () => JSON.parse(await kube('get', 'daemonsets', '-l', 'awaken.dev/image-warmup=true', '-o', 'json')).items,
      (holders) => holders.some((holder) => holder.metadata.labels?.['awaken.dev/image-warmup'] === 'true' && holder.spec.template.spec.containers.some((container) => container.image === process.env.AWAKEN_LAZY_SANDBOX_IMAGE) && holder.status?.desiredNumberScheduled > 0 && holder.status?.numberReady === holder.status?.desiredNumberScheduled && holder.status?.observedGeneration >= holder.metadata.generation),
      'actual native node image cache holder is Ready', { timeoutMs: 180_000, pollMs: 500 });
    record({ label: 'image-prewarm', duration_ms: performance.now() - started, holders });
  }
  if (startupSamples) {
    for (let index = 0; index < startupSamples; index += 1) {
      const id = await session(normalEnv);
      // C1 first physical call => no Session Pod before the tool; warming the
      // Session ahead of this clock cannot satisfy the on_tool_use measurement.
      assert.equal((await scopedPods(id)).length, 0, 'on_tool_use remains deferred');
      record({ label: 'startup-sample', sample: index + 1, session_id: id, repeat: false });
      await bashTurn(id);
      if (index === 0 || index === startupSamples - 1) {
        record({ label: 'startup-sample', sample: index + 1, session_id: id, repeat: true });
        await bashTurn(id);
      }
    }
    evidence.passed = true;
    return;
  }

  const available = evidence.storage_profile === 'local-static'
    ? JSON.parse(await kube('get', 'pv', '-o', 'json')).items.filter((p) => p.spec.storageClassName === 'awaken-local') : null;
  if (available) {
    // V1 empty preallocated PVs + A only uses pinned reads => archive A leaves
    // identical Available PV UIDs and no claims. V2 B's later first physical
    // tool binds one of those exact PVs. Archive must not invent a Sandbox.
    assert.ok(available.length > 0 && available.every((p) => p.status.phase === 'Available' && !p.spec.claimRef));
    const abandoned = await session(normalEnv); await readTurn(abandoned, 'unused-volume-session');
    assert.equal(JSON.parse(await kube('get', 'pvc', '-o', 'json')).items.length, 0, 'V1 pinned reads do not create claims');
    await client.beta.sessions.archive(abandoned, { betas });
    owned.sessions.splice(owned.sessions.indexOf(abandoned), 1);
    const after = JSON.parse(await kube('get', 'pv', '-o', 'json')).items.filter((p) => p.spec.storageClassName === 'awaken-local');
    const identities = (pvs) => pvs.map((p) => ({ name: p.metadata.name, uid: p.metadata.uid, phase: p.status.phase, claim: p.spec.claimRef ?? null })).sort((a, b) => a.name.localeCompare(b.name));
    assert.deepEqual(identities(after), identities(available));
    assert.equal((await scopedPods(abandoned)).length, 0);
    record({ label: 'unused-volumes-preserved-after-archive', session_id: abandoned, pvs: identities(after) });
  }
  const cold = await session(normalEnv); await readTurn(cold, 'cold-session');
  const warm = await session(normalEnv); await readTurn(warm, 'warm-session');
  // P1 complete pinned instructions are already in Agent.system; P2 facts.md
  // is unknown in cold context, recovery.md is unknown in warm context;
  // P3 cold/warm same Session. P1+P2 at
  // both P3 states => apply method without rereading SKILL.md, read reference,
  // deliver actual new bytes and reply, no Bash/Pod. Existing cold and restored
  // cases own metadata-only disclosure and missing-reference reads. Same SDK,
  // registry, read tool and Session helper; no loaded-state extension.
  const preloaded = await session(normalEnv, { type: 'agent_with_overrides', id: agent.id,
    version: agent.version, system: 'You help a Chinese SME owner. The following are the complete instructions of the pinned startup-diagnosis Skill.\n' + instructions });
  await readTurn(preloaded, 'preloaded-cold-session', true);
  await readTurn(preloaded, 'preloaded-warm-session', true);
  await clientQuestionTurn(normalEnv, agent, instructions);
  let historical;
  if (upgradeImage) {
    // U1 old committed runtime creates a retained Pod and writes state; U2
    // replacement runtime must adopt the same UID and execute pinned script
    // against that state. U3 no historical creation in the new cold path.
    const id = await session(normalEnv); await bashTurn(id);
    const pods = await scopedPods(id); assert.equal(pods.length, 1);
    assert.ok(pods[0].init_containers?.some((item) => item.name === 'continuation-init'));
    historical = { id, uid: pods[0].uid }; record({ label: 'historical-before-upgrade', ...historical });
  }
  const before = JSON.parse((await execute('docker', ['inspect', serviceContainer], { timeout: 15_000 })).stdout)[0];
  assert.equal(before.Config.Labels?.['awaken.test.case'], 'skill-lazy-real', 'restart only owned fixture');
  if (upgradeImage) {
    assert.equal(before.HostConfig.NetworkMode, 'host');
    assert.deepEqual(before.Config.Entrypoint, ['/usr/local/bin/awaken']);
    assert.equal(before.Config.Cmd[0], 'all-in-one');
    await execute('docker', ['stop', '--time', '15', before.Id], { timeout: 45_000 });
    await execute('docker', ['rm', before.Id], { timeout: 15_000 });
    await execute('docker', ['run', '-d', '--network', 'host', '--name', serviceContainer,
      '--label', 'awaken.test.case=skill-lazy-real', '-e', 'KUBECONFIG=/etc/awaken/kubeconfig',
      ...before.HostConfig.Binds.flatMap((binding) => ['-v', binding]),
      '--entrypoint', '/usr/local/bin/awaken', upgradeImage, ...before.Config.Cmd], { timeout: 30_000 });
    directoryInit = false;
  } else await execute('docker', ['restart', '--time', '15', before.Id], { timeout: 45_000 });
  await waitForValue(async () => {
    try { return (await fetch(`${baseURL}/v1/config/workspace-context`)).ok; }
    catch { return false; }
  }, (ready) => ready, 'same service restored after restart', { timeoutMs: 90_000, pollMs: 500 });
  await readTurn(cold, 'cold-restored-session');
  if (historical) {
    directoryInit = true; await bashTurn(historical.id); directoryInit = false;
    const pods = await scopedPods(historical.id); assert.equal(pods.length, 1);
    assert.equal(pods[0].uid, historical.uid, 'U2 same retained historical Pod');
    record({ label: 'historical-after-upgrade', session_id: historical.id, uid: pods[0].uid });
  }
  await bashTurn(cold);
  if (available) {
    const pods = await scopedPods(cold); assert.equal(pods.length, 1);
    const claimName = pods[0].volumes.find((v) => v.name === 'continuation-state')?.persistentVolumeClaim?.claimName;
    assert.ok(claimName);
    const claim = JSON.parse(await kube('get', 'pvc', claimName, '-o', 'json'));
    const pv = JSON.parse(await kube('get', 'pv', claim.spec.volumeName, '-o', 'json'));
    assert.ok(available.some((p) => p.metadata.uid === pv.metadata.uid && p.metadata.name === pv.metadata.name));
    assert.equal(pv.status.phase, 'Bound'); assert.equal(pv.spec.claimRef.uid, claim.metadata.uid);
    record({ label: 'next-session-claims-original-unused-volume', session_id: cold, claim, pv });
  }
  const failed = await session(await environment(true));
  await readTurn(failed, 'unavailable-sandbox-before-tool');
  await bashTurn(failed, true);
  evidence.passed = true;
}

main().catch((error) => { evidence.error = String(error); process.exitCode = 1; console.error(String(error)); })
  .finally(async () => {
    fs.mkdirSync(output, { recursive: true, mode: 0o700 });
    for (const id of owned.sessions) {
      try { await client.beta.sessions.archive(id, { betas }); }
      catch (error) { evidence.cleanup_error ??= []; evidence.cleanup_error.push({ session_id: id, error: String(error) }); process.exitCode = 1; }
    }
    fs.writeFileSync(path.join(output, 'evidence.json'), JSON.stringify(evidence, null, 2));
    console.log(`Evidence: ${path.join(output, 'evidence.json')}; passed=${evidence.passed}`);
  });
