import { test, expect } from '@playwright/test';
import { spawnSync, SpawnSyncOptions } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const DOTNET_SDK_COMMIT = '71aeb58759fb5ae6d9765e061f4ef8d9f25e352d';
const GO_IMAGE = 'golang:1.24.4';
const DOTNET_IMAGE = 'mcr.microsoft.com/dotnet/sdk:8.0';

test.describe.configure({ mode: 'serial' });
test.skip(process.platform !== 'linux', 'official SDK interop uses Docker host networking on Linux');

test.beforeAll(() => {
  runChecked('docker', ['version']);
});

test.describe('official A2A SDK interoperability', () => {
  test('official Go SDK client interoperates with Awaken A2A HTTP+JSON v1 server', async () => {
    const workspace = createTempWorkspace('go');
    try {
      copyFixture('go-client', path.join(workspace, 'app'));

      const stdout = runDocker(
        GO_IMAGE,
        workspace,
        '/work/app',
        `export PATH=/usr/local/go/bin:$PATH && go mod tidy && go run . http://127.0.0.1:38080`,
      );

      const result = parseJsonLine(stdout);
      expect(result.cardName).toBe('default');
      expect(result.cardUrl).toBe('http://127.0.0.1:38080/v1/a2a');
      expect(result.protocolBinding).toBe('HTTP+JSON');
      expect(result.protocolVersion).toBe('1.0');
      expect(result.taskId).toContain('go-interop-');
      expect(String(result.initialState)).toMatch(/^TASK_STATE_/);
      expect(result.finalState).toBe('TASK_STATE_COMPLETED');
      expect(result.message).toContain('hello from go');
    } finally {
      cleanupWorkspace(workspace);
    }
  });

  test('official .NET SDK client interoperates with Awaken A2A HTTP+JSON v1 server', async () => {
    const workspace = createTempWorkspace('dotnet');
    try {
      copyFixture('dotnet-client', path.join(workspace, 'app'));
      runChecked('git', ['clone', 'https://github.com/a2aproject/a2a-dotnet', path.join(workspace, 'a2a-dotnet')]);
      runChecked('git', ['-C', path.join(workspace, 'a2a-dotnet'), 'checkout', DOTNET_SDK_COMMIT]);

      const stdout = runDocker(
        DOTNET_IMAGE,
        workspace,
        '/work/app',
        [
          'dotnet restore --configfile /work/a2a-dotnet/nuget.config -p:TargetFramework=net8.0',
          'dotnet run --no-restore -f net8.0 -- http://127.0.0.1:38080',
        ].join(' && '),
      );

      const result = parseJsonLine(stdout);
      expect(result.cardName).toBe('default');
      expect(result.cardUrl).toBe('http://127.0.0.1:38080/v1/a2a');
      expect(result.protocolBinding).toBe('HTTP+JSON');
      expect(result.protocolVersion).toBe('1.0');
      expect(result.taskId).toContain('dotnet-interop-');
      expect(String(result.initialState)).toMatch(/^TASK_STATE_/);
      expect(result.finalState).toBe('TASK_STATE_COMPLETED');
      expect(result.message).toContain('hello from dotnet');
    } finally {
      cleanupWorkspace(workspace);
    }
  });
});

function fixtureRoot() {
  return path.join(process.cwd(), 'interop');
}

function createTempWorkspace(prefix: string) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `awaken-a2a-${prefix}-`));
}

function copyFixture(name: string, destination: string) {
  fs.cpSync(path.join(fixtureRoot(), name), destination, { recursive: true });
}

function cleanupWorkspace(workspace: string) {
  try {
    fs.rmSync(workspace, { recursive: true, force: true });
  } catch {
    runChecked('docker', [
      'run',
      '--rm',
      '-v',
      `${workspace}:/work`,
      'alpine:3.21',
      'sh',
      '-c',
      `chown -R ${process.getuid()}:${process.getgid()} /work`,
    ]);
    fs.rmSync(workspace, { recursive: true, force: true });
  }
}

function runDocker(image: string, workspace: string, workdir: string, command: string) {
  return runChecked('docker', [
    'run',
    '--rm',
    '--network',
    'host',
    '--user',
    `${process.getuid()}:${process.getgid()}`,
    '-e',
    'HOME=/work/.home',
    '-v',
    `${workspace}:/work`,
    '-w',
    workdir,
    image,
    'bash',
    '-c',
    `mkdir -p /work/.home && ${command}`,
  ]);
}

function runChecked(command: string, args: string[], options?: SpawnSyncOptions) {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    maxBuffer: 50 * 1024 * 1024,
    ...options,
  });

  if (result.status !== 0) {
    throw new Error(
      [
        `command failed: ${command} ${args.join(' ')}`,
        result.stdout?.trim(),
        result.stderr?.trim(),
      ]
        .filter(Boolean)
        .join('\n\n'),
    );
  }

  return result.stdout;
}

function parseJsonLine(output: string) {
  const line = output
    .trim()
    .split('\n')
    .reverse()
    .find(candidate => candidate.trim().startsWith('{'));

  if (!line) {
    throw new Error(`missing JSON result in output:\n${output}`);
  }

  return JSON.parse(line);
}
