import test from 'node:test';
import assert from 'node:assert/strict';
import {DatabaseSync} from 'node:sqlite';
import {CURRENT_WORKER_ROWS_QUERY, standaloneWorkerCargoFeatures, standaloneWorkerSandboxConfig} from './standalone_host_worker.mjs';
import {cargoArguments} from '../cargo_binary.mjs';
import {resolveAcpLiveProfile} from '../acp_live_profile.mjs';

test('standalone Worker grants local Git only to the process that owns Repository I/O', () => {
  // R1 Linux scoped execution => the separately-built companion owns the aperture.
  // R2 native Local execution => the Worker needs the Local transport aperture.
  // Neither rule selects Host storage fixtures or changes a production build.
  for (const platform of ['linux','darwin','win32']) for (const runtime of ['codex','claude']) {
    const {scopedHost}=resolveAcpLiveProfile({profile:'worker-host-fixture',runtime,platform});
    const args=cargoArguments({packageName:'awaken-worker',targetName:'awaken-worker',
      features:standaloneWorkerCargoFeatures(scopedHost)});
    const flag=args.indexOf('--features');
    if (platform === 'linux') assert.equal(flag,-1);
    else {
      assert.notEqual(flag,-1);
      assert.equal(args[flag+1],'awaken-execution-composition/test-support');
    }
    assert.ok(!args.join(',').includes('awaken-session-executor/test-support'));
  }
  assert.equal(cargoArguments({packageName:'awaken-worker',targetName:'awaken-worker'})
    .includes('--features'),false);
});

test('standalone Worker reuses one sandbox field projection', () => {
  // Causes: S1 caller supplies the canonical sandbox field object; S2 a value
  // is absent; S3 an older caller supplies only tier/image/runtime; S4 trusted
  // host caller has no Provider bundle. Effects: E1 encode every supplied field
  // exactly once; E2 omit absent values; E3 preserve the legacy container
  // projection; E4 preserve trusted-host admission. Rules W1=S1, W2=S1+S2,
  // W3=S3, W4=S4. The real GKE journey proves these bytes are accepted by the
  // production Worker; this unit test prevents Control/Worker config drift.
  const fields={sandbox_tier:'k8s',container_image:'registry/image:tag',
    k8s_namespace:'cowork-test',k8s_image_pull_secrets:['registry-auth'],
    'k8s_continuation_volume.size':'1Gi',unused:undefined};
  assert.deepEqual(standaloneWorkerSandboxConfig({sandboxFields:fields},'native'),[
    'sandbox_tier = "k8s"','container_image = "registry/image:tag"',
    'k8s_namespace = "cowork-test"','k8s_image_pull_secrets = ["registry-auth"]',
    'k8s_continuation_volume.size = "1Gi"',
  ]);
  assert.deepEqual(standaloneWorkerSandboxConfig({sandboxTier:'docker',image:'image'},'claude'),[
    'sandbox_tier = "docker"','container_image = "image"','acp_clis = ["claude"]',
  ]);
  assert.deepEqual(standaloneWorkerSandboxConfig(undefined,'native'),[
    'sandbox_tier = "local"','sandbox_trusted_host_enabled = true','acp_clis = []',
  ]);
});

test('registration observation selects current generations without erasing history', () => {
  // R1 old Ready + newer Starting/Ready/Dead => only newer state, never fallback.
  // R2 independent Worker IDs + reversed insertion order => one maximum per ID.
  // R3 reading must preserve every historical row; PostgreSQL uses this exact
  // query in the Cowork crash/reopen journey, not a separate selector.
  for (const state of ['starting','ready','dead']) for (const reverse of [false,true]) {
    const db=new DatabaseSync(':memory:');
    try {
      db.exec(`CREATE TABLE worker_registry_worker(worker_id TEXT,incarnation_id TEXT,
        generation INTEGER,state TEXT,expires_at_ms INTEGER,manifest_json TEXT,
        credential_observations_json TEXT,observation_sequence INTEGER)`);
      const rows=[['a','old',1,'ready'],['a','new',2,state],['b','other',1,'ready']];
      for (const row of reverse ? rows.toReversed() : rows) {
        db.prepare('INSERT INTO worker_registry_worker VALUES(?,?,?,?,0,?, ?,0)').run(...row,'{}','[]');
      }
      assert.deepEqual(db.prepare(CURRENT_WORKER_ROWS_QUERY).all().map(r=>[r.worker_id,r.incarnation_id,r.generation,r.state]),
        [['a','new',2,state],['b','other',1,'ready']]);
      assert.equal(db.prepare('SELECT COUNT(*) AS count FROM worker_registry_worker').get().count,3);
    } finally {db.close();}
  }
});
