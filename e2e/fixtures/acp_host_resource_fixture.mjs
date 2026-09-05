// This peer interprets the same resource mission used by the live ACP gate.
// It never receives physical Session paths through a fixture-side back door.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { execFileSync } from 'node:child_process';

const role = process.argv[2];
const args = process.argv.slice(3);
if (args.includes('--version')) {
  console.log(`${role} 9.9.9`);
  process.exit(0);
}
if (args[0] === 'login' || args[0] === 'auth') {
  console.log(role === 'claude' ? '{"loggedIn":true}' : 'Logged in using ChatGPT');
  process.exit(0);
}
const send = (message) => console.log(JSON.stringify({ jsonrpc: '2.0', ...message }));
const sessionId = `fixture-${process.pid}`;
let cwd;
for await (const line of readline.createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  let result;
  switch (request.method) {
    case 'initialize':
      result = { protocolVersion: 1, agentCapabilities: {} };
      break;
    case 'session/new':
      cwd = request.params.cwd;
      assert.equal(fs.realpathSync(cwd), fs.realpathSync(process.cwd()), 'ACP cwd equals process cwd');
      result = { sessionId };
      break;
    case 'session/prompt': {
      const text = request.params.prompt.map((block) => block.text ?? '').join('\n');
      if (!text.includes('AWK_RESOURCE_MISSION')) {
        const marker = /CODEX-ACP-READY-\d+/u.exec(text)?.[0];
        assert.ok(marker);
        send({ method: 'session/update', params: { sessionId, update: {
          sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: marker },
        } } });
      } else {
        const variable = (name) => {
          const value = new RegExp(`^AWK_${name}=(.+)$`, 'mu').exec(text)?.[1];
          assert.ok(value && !value.includes('{{awaken.'), `rendered ${name}`);
          assert.ok(!path.isAbsolute(value), 'TrustedHost prompt contains relative paths');
          return value;
        };
        const workspace = variable('WORKSPACE');
        const memory = variable('MEMORY');
        const outputs = variable('OUTPUTS');
        const file = /A file is mounted read-only at `([^`]+)`/u.exec(text)?.[1];
        const repository = /A git repository is checked out at `([^`]+)`/u.exec(text)?.[1];
        assert.ok(file && repository, 'default File and Repository instructions');
        const seed = fs.readFileSync(file, 'utf8');
        const repoSeed = fs.readFileSync(path.join(repository, 'README.md'), 'utf8');
        const memorySeed = fs.readFileSync(path.join(memory, 'seed.md'), 'utf8');
        assert.ok(fs.statSync(workspace).isDirectory());
        assert.ok(!text.includes(cwd), 'host root is absent from the model input');
        const resultName = /^AWK_RESULT=(result-(?:default|nested))$/mu.exec(text)?.[1];
        assert.ok(resultName);
        const head = execFileSync('git', ['-C', repository, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
        assert.match(head, /^[0-9a-f]{40}$/u);
        const attempt = [...text.matchAll(/^AWK_ATTEMPT=(.+)$/gmu)].at(-1)?.[1];
        assert.ok(attempt);
        const combined = [seed, memorySeed, repoSeed, head, attempt].join('\n');
        const contention = [...text.matchAll(/^AWK_CONTEND=(.+)$/gmu)].at(-1)?.[1];
        if (contention) {
          for (const name of ['shared-a-identical.md', 'shared-z-conflict.md']) {
            assert.equal(fs.readFileSync(path.join(memory, name), 'utf8'), memorySeed,
              'both physical copy projections retain the same initial head');
          }
          fs.writeFileSync(path.join(memory, 'shared-a-identical.md'), `${memorySeed}\n${contention}`);
          fs.writeFileSync(path.join(memory, 'shared-z-conflict.md'), combined);
        }
        const previous = [...text.matchAll(/^AWK_PREVIOUS=(.+)$/gmu)].at(-1)?.[1];
        if (previous) {
          const prior = [seed, memorySeed, repoSeed, head, previous].join('\n');
          assert.equal(fs.readFileSync(path.join(memory, resultName + '.md'), 'utf8'), prior,
            'Memory prior edit survives Worker replacement');
          assert.equal(fs.readFileSync(path.join(repository, resultName + '.txt'), 'utf8'), prior,
            'Repository prior edit survives Worker replacement');
        }
        fs.writeFileSync(path.join(memory, resultName + '.md'), combined);
        fs.writeFileSync(path.join(repository, resultName + '.txt'), combined);
        fs.mkdirSync(outputs, { recursive: true });
        fs.writeFileSync(path.join(outputs, 'result.txt'), combined);
        execFileSync('git', ['-C', repository, 'add', '-N', resultName + '.txt']);
        fs.writeFileSync(path.join(outputs, 'repository.patch'),
          execFileSync('git', ['-C', repository, 'diff', '--', resultName + '.txt']));
        send({ method: 'session/update', params: { sessionId, update: {
          sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: `RESOURCE_OK ${seed} ${attempt}` },
        } } });
      }
      result = { stopReason: 'end_turn' };
      break;
    }
    default:
      if (request.id === undefined) continue;
      throw new Error(`unexpected ACP method ${request.method}`);
  }
  send({ id: request.id, result });
}
