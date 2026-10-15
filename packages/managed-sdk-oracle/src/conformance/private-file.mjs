import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

// Supply the DACL to CreateNew itself: chmod after writing would expose the
// contents through the inherited ACL, and Node's Windows mode bits are not ACLs.
const WINDOWS_PRIVATE_WRITE = `
$ErrorActionPreference = 'Stop'
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
$request = [Console]::In.ReadToEnd() | ConvertFrom-Json
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User
$security = [Security.AccessControl.FileSecurity]::new()
$security.SetOwner($owner)
$security.SetAccessRuleProtection($true, $false)
$security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
  $owner, 'FullControl', 'Allow'))
$stream = $null
$created = $false
try {
  $stream = [IO.FileStream]::new($request.path, [IO.FileMode]::CreateNew,
    [Security.AccessControl.FileSystemRights]::Write, [IO.FileShare]::None,
    4096, [IO.FileOptions]::None, $security)
  $created = $true
  $bytes = [Text.Encoding]::UTF8.GetBytes($request.contents)
  $stream.Write($bytes, 0, $bytes.Length)
  $stream.Dispose()
} catch {
  if ($null -ne $stream) { $stream.Dispose() }
  if ($created) { [IO.File]::Delete($request.path) }
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
}
`;

export function writePrivateFile(file, contents) {
  if (process.platform === 'win32') {
    const result = spawnSync(path.join(process.env.SystemRoot, 'System32',
      'WindowsPowerShell', 'v1.0', 'powershell.exe'), [
      '-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand',
      Buffer.from(WINDOWS_PRIVATE_WRITE, 'utf16le').toString('base64'),
    ], {
      input: JSON.stringify({ path: path.resolve(file), contents }),
      encoding: 'utf8', windowsHide: true,
    });
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`private file creation failed: ${result.stderr}`);
    return;
  }
  const descriptor = fs.openSync(file, 'wx', 0o600);
  try {
    fs.writeFileSync(descriptor, contents);
  } catch (error) {
    fs.rmSync(file);
    throw error;
  } finally {
    fs.closeSync(descriptor);
  }
}
