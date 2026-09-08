import assert from 'node:assert/strict';
import {test} from 'node:test';
import {withoutControlAuthority} from './process_environment.mjs';

test('Worker child environment excludes Control authority and preserves execution configuration', () => {
  // Decision rules E1 provider key => absent; E2 database/sealing/config
  // authority => absent; E3 execution paths/topology/worker identity => retained.
  // Mixed input checks simultaneous causes and no mutation of the parent.
  const denied = ['API_KEY','deepseek_api_key','COWORK_DATABASE_URL',
    'DATABASE_URL','AWAKEN_RESOURCE_DATABASE_URL','COWORK_POSTGRES_CONTAINER',
    'PGPASSWORD','PGSERVICEFILE','AWAKEN_SEAL_KEY_FILE','AWAKEN_CONFIG_PATH'];
  const allowed = {PATH:'/fixture/bin',HOME:'/worker',AWAKEN_DATA_DIR:'/worker',
    AWAKEN_WORKER_ID:'independent',AWAKEN_UPSTREAM_URL:'https://control',
    CONTAINER_HOST:'unix:///worker/podman.sock'};
  const parent = {...allowed,...Object.fromEntries(denied.map(key => [key,'private']))};
  assert.deepEqual(withoutControlAuthority(parent),allowed);
  assert.equal(Object.keys(parent).length,denied.length+Object.keys(allowed).length);
  assert.deepEqual(withoutControlAuthority({}),{});
});
