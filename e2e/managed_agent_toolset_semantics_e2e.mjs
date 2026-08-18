// Node-only official TypeScript SDK agent_toolset_20260401 semantics. These are
// the exact Bash/file implementations EnvironmentWorker installs by default.

import assert from 'node:assert/strict';
import { mkdtempSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { betaAgentToolset20260401 } from '@anthropic-ai/sdk/tools/agent-toolset/node';
import { pass } from './harness.mjs';

const workdir = mkdtempSync(join(tmpdir(), 'awaken-agent-toolset-'));
const tools = betaAgentToolset20260401({ workdir, maxFileBytes: 64 });
const byName = Object.fromEntries(tools.map((tool) => [tool.name, tool]));

try {
  assert.deepEqual(
    Object.keys(byName).sort(),
    ['bash', 'edit', 'glob', 'grep', 'read', 'write'],
    'the official helper exposes the complete documented Node toolset',
  );
  assert.match(await byName.write.run({ file_path: 'nested/note.txt', content: 'alpha\nbeta\n' }), /wrote/);
  assert.equal(await byName.read.run({ file_path: 'nested/note.txt' }), 'alpha\nbeta\n');
  assert.match(await byName.edit.run({
    file_path: 'nested/note.txt',
    old_string: 'beta',
    new_string: 'gamma',
  }), /edited/);
  assert.match(await byName.grep.run({ pattern: 'gamma', path: 'nested' }), /note\.txt/);
  assert.match(await byName.glob.run({ pattern: '**/*.txt' }), /nested\/note\.txt/);
  assert.equal(await byName.bash.run({ command: 'pwd; printf sdk-bash' }), `${workdir}\nsdk-bash`);
  pass('AT-01 Bash and every file tool execute their happy path');

  for (const action of [
    () => byName.read.run({ file_path: '/etc/passwd' }),
    () => byName.write.run({ file_path: '../escape.txt', content: 'no' }),
    () => byName.read.run({ file_path: 'nested' }),
    () => byName.edit.run({
      file_path: 'nested/note.txt', old_string: 'missing', new_string: 'no',
    }),
  ]) {
    await assert.rejects(action);
  }
  symlinkSync('/etc/passwd', join(workdir, 'outside-link'));
  await assert.rejects(
    () => byName.read.run({ file_path: 'outside-link' }),
    /escapes workdir/,
  );
  await byName.write.run({ file_path: 'large.txt', content: 'x'.repeat(65) });
  await assert.rejects(() => byName.read.run({ file_path: 'large.txt' }), /exceeds 64-byte limit/);
  pass('AT-02 traversal, symlink escape, non-file, missing edit target, and size cap fail closed');

  await assert.rejects(
    () => byName.bash.run({ command: 'sleep 5', timeout_ms: 20 }),
    /timed out after 20ms/,
  );
  const controller = new AbortController();
  const aborted = byName.bash.run({ command: 'sleep 5' }, { signal: controller.signal });
  setTimeout(() => controller.abort(), 20);
  await assert.rejects(aborted, /aborted/i);
  assert.equal(
    await byName.bash.run({ command: 'printf recovered' }),
    'recovered',
    'timeout/abort discard the poisoned shell and the next invocation is clean',
  );
  await assert.rejects(() => byName.bash.run({ command: 'exit 7' }));
  pass('AT-03 Bash timeout, cancellation, non-zero exit, and post-failure recovery are deterministic');

  console.log('E2E PASS: official TypeScript agent toolset success and non-happy-path semantics.');
} finally {
  await Promise.allSettled(tools.map((tool) => tool.close?.()));
  rmSync(workdir, { recursive: true, force: true });
}
