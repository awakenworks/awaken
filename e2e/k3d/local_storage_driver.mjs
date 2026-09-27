// Standard Kubernetes PVC consumers: no Awaken-specific storage API or pool.
// Cause/effect rules stay beside the corresponding live assertions below.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { waitForValue } from '../harness.mjs';

const run = promisify(execFile);
assert.equal(process.argv.length, 2, 'environment configuration only');
const context = process.env.AWAKEN_LOCAL_STORAGE_CONTEXT;
assert.ok(context?.startsWith('k3d-awaken-pvc-'), 'owned isolated fixture required');
const output = path.resolve(process.env.AWAKEN_EVAL_ARTIFACT_DIR);
const evidence = { context, source_commit: process.env.AWAKEN_LOCAL_STORAGE_COMMIT, observations: [], passed: false };
function record(value) { evidence.observations.push(value); fs.writeFileSync(path.join(output, 'evidence.json'), JSON.stringify(evidence, null, 2)); }
async function kube(...args) { return (await run('kubectl', ['--context', context, ...args], { timeout: args.includes('--wait=true') || args.includes('rollout') ? 100_000 : 20_000, maxBuffer: 8 * 1024 * 1024 })).stdout; }
async function apply(object) {
  const file = path.join(output, 'request.json'); fs.writeFileSync(file, JSON.stringify(object));
  await kube('apply', '-f', file);
}
const list = async (kind, namespace) => JSON.parse(await kube(...(namespace ? ['-n', namespace] : []), 'get', kind, '-o', 'json')).items;
async function ready(name, namespace) {
  return waitForValue(async () => (await list('pods', namespace)).find((p) => p.metadata.name === name),
    (p) => p?.status?.conditions?.some((c) => c.type === 'Ready' && c.status === 'True'),
    `consumer ${namespace}/${name} becomes Ready`, { timeoutMs: 90_000, pollMs: 200 });
}
function claim(name, namespace, size = '128Mi', storageClassName = 'awaken-local') {
  return { apiVersion: 'v1', kind: 'PersistentVolumeClaim', metadata: { name, namespace },
    spec: { accessModes: ['ReadWriteOnce'], volumeMode: 'Filesystem', storageClassName, resources: { requests: { storage: size } } } };
}
function pod(name, namespace, pvc, command, image) {
  return { apiVersion: 'v1', kind: 'Pod', metadata: { name, namespace }, spec: {
    automountServiceAccountToken: false,
    terminationGracePeriodSeconds: 1,
    securityContext: { runAsUser: 10001, runAsGroup: 10001, fsGroup: 10001 },
    containers: [{ name: 'consumer', image, imagePullPolicy: 'IfNotPresent', command: ['sh', '-ec', `${command}; exec sleep 3600`], volumeMounts: [{ name: 'data', mountPath: '/data' }] }],
    volumes: [{ name: 'data', persistentVolumeClaim: { claimName: pvc } }] } };
}
async function consume(name, namespace, size, command, image, storageClassName = 'awaken-local') {
  const start = performance.now(); await apply(claim(name, namespace, size, storageClassName));
  await apply(pod(name, namespace, name, command, image));
  const p = await ready(name, namespace); const c = (await list('pvc', namespace)).find((v) => v.metadata.name === name);
  assert.equal(c.status.phase, 'Bound');
  record({ label: 'consume', namespace, name, storage_class: storageClassName, observed_ready_ms: performance.now() - start, pod: p, claim: c });
  return c;
}

async function main() {
  fs.mkdirSync(output, { recursive: true });
  // P1 pre-mounted empty disks => four Available PVs before any consumer Pod.
  const initial = await waitForValue(async () => (await list('pv')).filter((p) => p.spec.storageClassName === 'awaken-local'),
    (pvs) => pvs.length === 4 && pvs.every((p) => p.status.phase === 'Available'),
    'preallocated independent filesystems become Available PVs', { timeoutMs: 90_000, pollMs: 250 });
  assert.equal((await list('pods', 'awaken-storage')).length, 1, 'one node controller, no per-PVC helpers');
  record({ label: 'available-before-consumers', pvs: initial });
  const helper = JSON.parse(await kube('-n', 'kube-system', 'get', 'configmap', 'local-path-config', '-o', 'json')).data['helperPod.yaml'];
  const image = /^\s*image:\s*([^\s]+)\s*$/m.exec(helper)?.[1].replaceAll('"', ''); assert.ok(image);
  for (const name of ['pvc-a', 'pvc-b']) await apply({ apiVersion: 'v1', kind: 'Namespace', metadata: { name } });
  // P2 namespaces + distinct capacities => ordinary PVCs bind exclusive PVs.
  // Before consumers, WFFC claims remain Pending despite available capacity.
  await apply(claim('session', 'pvc-a', '1Gi')); await apply(claim('cache', 'pvc-b', '128Mi'));
  assert.equal((await list('pvc', 'pvc-a'))[0].status.phase, 'Pending');
  assert.equal((await list('pvc', 'pvc-b'))[0].status.phase, 'Pending');
  const a = await consume('session', 'pvc-a', '1Gi', "printf '%s' owner-a > /data/identity", image);
  const b = await consume('cache', 'pvc-b', '128Mi', "printf '%s' owner-b > /data/identity", image);
  assert.notEqual(a.spec.volumeName, b.spec.volumeName);
  const bound = (await list('pv')).filter((p) => [a.spec.volumeName, b.spec.volumeName].includes(p.metadata.name));
  assert.ok(bound.every((p) => p.spec.nodeAffinity && p.spec.claimRef?.uid));
  // P3 Pod replacement and controller restart => same PVC UID/PV/data.
  await kube('-n', 'pvc-a', 'delete', 'pod', 'session', '--wait=true');
  await apply(pod('restored', 'pvc-a', 'session', 'test "$(cat /data/identity)" = owner-a', image)); await ready('restored', 'pvc-a');
  await kube('-n', 'awaken-storage', 'rollout', 'restart', 'daemonset/awaken-local-storage');
  await kube('-n', 'awaken-storage', 'rollout', 'status', 'daemonset/awaken-local-storage', '--timeout=90s');
  const restored = (await list('pvc', 'pvc-a')).find((p) => p.metadata.name === 'session');
  assert.equal(restored.metadata.uid, a.metadata.uid); assert.equal(restored.spec.volumeName, a.spec.volumeName);
  assert.equal((await kube('-n', 'pvc-a', 'exec', 'restored', '--', 'cat', '/data/identity')).trim(), 'owner-a');
  record({ label: 'restored-same-volume', claim: restored });
  // P4 legacy storage stays usable; classes are not redirected/migrated.
  await consume('legacy', 'pvc-a', '64Mi', 'printf legacy > /data/legacy', image, 'local-path');
  const classes = await list('storageclass');
  assert.equal(classes.find((c) => c.metadata.name === 'local-path').metadata.annotations['storageclass.kubernetes.io/is-default-class'], 'true');
  // P5 exhaustion => new consumer Pending, never another client's data/PV.
  await consume('third', 'pvc-a', '128Mi', 'printf third > /data/identity', image);
  await consume('fourth', 'pvc-b', '128Mi', 'printf fourth > /data/identity', image);
  await apply(claim('waiting', 'pvc-b')); await apply(pod('waiting', 'pvc-b', 'waiting', 'test ! -e /data/identity', image));
  const pending = await waitForValue(async () => (await list('pods', 'pvc-b')).find((p) => p.metadata.name === 'waiting'),
    (p) => p?.status?.conditions?.some((c) => c.type === 'PodScheduled' && c.status === 'False' && c.reason === 'Unschedulable'),
    'exhausted pool remains unscheduled', { timeoutMs: 60_000, pollMs: 250 });
  record({ label: 'exhausted', pod: pending });
  // P6 explicit PVC release => upstream cleanup, new PV UID, then a waiting
  // ordinary consumer binds it with no previous identity file visible.
  const prior = (await list('pv')).find((p) => p.metadata.name === b.spec.volumeName);
  await kube('-n', 'pvc-b', 'delete', 'pod', 'cache', '--wait=true');
  await kube('-n', 'pvc-b', 'delete', 'pvc', 'cache', '--wait=true');
  await ready('waiting', 'pvc-b');
  const recycled = (await list('pv')).find((p) => p.metadata.name === b.spec.volumeName);
  assert.ok(recycled); assert.notEqual(recycled.metadata.uid, prior.metadata.uid);
  assert.equal((await list('pvc', 'pvc-b')).find((c) => c.metadata.name === 'waiting').spec.volumeName, b.spec.volumeName);
  record({ label: 'cleaned-before-reallocation', previous_uid: prior.metadata.uid, pv: recycled });
  evidence.passed = true;
}
main().catch((error) => { evidence.error = String(error); process.exitCode = 1; console.error(String(error)); })
  .finally(() => { fs.mkdirSync(output, { recursive: true }); fs.writeFileSync(path.join(output, 'evidence.json'), JSON.stringify(evidence, null, 2)); console.log(`Evidence: ${path.join(output, 'evidence.json')}; passed=${evidence.passed}`); });
