import { spawnSync } from 'node:child_process';

export function bwrapAvailable() {
  return spawnSync('bwrap', ['--unshare-user', '--ro-bind', '/', '/', '--', 'true'], {
    stdio: 'ignore',
  }).status === 0;
}

export function requireOrSkipBwrap() {
  if (bwrapAvailable()) return true;
  if (process.env.AWAKEN_E2E_REQUIRE_BWRAP === '1') {
    throw new Error('required bwrap/unprivileged user namespace is unavailable');
  }
  console.log('E2E SKIP: bwrap/unprivileged userns unavailable on this host.');
  return false;
}
