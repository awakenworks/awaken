"""Native Windows S3 fixture; policy remains a caller-provided domain input."""
from __future__ import annotations

import json
import os
from pathlib import Path
import secrets
import subprocess
import time
import urllib.request

from native_services import free_port, windows_executable


def validate_minio_identity(document: dict, endpoint: str, data: Path) -> dict:
    if not isinstance(document, dict) or document.get('status') != 'success':
        raise ValueError('native MinIO readiness requires authenticated admin info')
    info = document.get('info', {})
    if not isinstance(info, dict): raise ValueError('native MinIO info is malformed')
    servers = info.get('servers', [])
    if not isinstance(info.get('deploymentID'), str) or not info['deploymentID'] or not isinstance(servers, list) or len(servers) != 1:
        raise ValueError('native MinIO deployment identity is missing or ambiguous')
    server = servers[0]
    if not isinstance(server, dict): raise ValueError('native MinIO server is malformed')
    drives = server.get('drives', [])
    if not isinstance(drives, list) or len(drives) != 1 or not isinstance(drives[0], dict):
        raise ValueError('native MinIO drive is missing or ambiguous')
    if (server.get('state') != 'online' or server.get('endpoint') != endpoint
            or len(drives) != 1 or drives[0].get('state') != 'ok'
            or not isinstance(drives[0].get('path'), str)
            or Path(drives[0]['path']).resolve() != data.resolve()
            or not isinstance(drives[0].get('uuid'), str) or not drives[0]['uuid']
            or not isinstance(server.get('version'), str) or not server['version']
            or not isinstance(server.get('commitID'), str) or not server['commitID']):
        raise ValueError('native MinIO endpoint/data do not belong to this fixture')
    return {'deployment_id': info['deploymentID'], 'endpoint': endpoint,
            'data_directory': str(data.resolve()), 'drive_uuid': drives[0]['uuid'],
            'version': server['version'], 'commit_id': server['commitID']}


class MinioService:
    def __init__(self, server: Path, client: Path, root: Path, output: Path, owner: str, policy: Path):
        if os.name != 'nt':
            raise RuntimeError('native service validation requires Windows')
        self.binaries = {'minio': windows_executable(server), 'mc': windows_executable(client)}
        self.root, self.output, self.owner, self.policy = root, output, owner, policy
        self.data = root / 'data'
        self.port, self.console = free_port(), free_port()
        if self.port == self.console:
            raise RuntimeError('native MinIO API and console ports must be distinct')
        self.endpoint = f'http://127.0.0.1:{self.port}'
        self.root_user = 'root-' + secrets.token_hex(8)
        self.root_secret = secrets.token_hex(24)
        self.user = 'app-' + secrets.token_hex(8)
        self.old_secret, self.new_secret = secrets.token_hex(24), secrets.token_hex(24)
        self.rotated = False
        self.identity = None
        self.process = None
        self.log = None
        self.environment = {key: value for key, value in os.environ.items()
                            if not key.upper().startswith(('MINIO_', 'MC_HOST_'))}
        self.environment.update(MINIO_ROOT_USER=self.root_user, MINIO_ROOT_PASSWORD=self.root_secret,
            MINIO_BROWSER='off', MC_HOST_owned=f'http://{self.root_user}:{self.root_secret}@127.0.0.1:{self.port}')

    def command(self, arguments: list[str]) -> bytes:
        result = subprocess.run([self.binaries['mc']['path'], '--config-dir', str(self.root / 'mc-config'), *arguments],
            env=self.environment, capture_output=True, stdin=subprocess.DEVNULL,
            timeout=15, creationflags=subprocess.CREATE_NO_WINDOW)
        if result.returncode:
            raise RuntimeError(f'owned native MinIO admin command failed with status {result.returncode}')
        return result.stdout

    def read_identity(self) -> dict:
        value = json.loads(self.command(['--json', 'admin', 'info', 'owned']))
        return validate_minio_identity(value, f'127.0.0.1:{self.port}', self.data)

    def start(self) -> dict:
        self.root.mkdir()
        self.log = (self.output / 'minio-server.log').open('ab')
        self.process = subprocess.Popen([self.binaries['minio']['path'], 'server', str(self.data),
            '--address', f'127.0.0.1:{self.port}', '--console-address', f'127.0.0.1:{self.console}'],
            env=self.environment, stdout=self.log, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NO_WINDOW)
        until = time.monotonic() + 45
        while time.monotonic() < until:
            if self.process.poll() is not None:
                raise RuntimeError('owned native MinIO exited before readiness')
            try:
                with urllib.request.urlopen(self.endpoint + '/minio/health/ready', timeout=1) as response:
                    if response.status == 200:
                        break
            except OSError:
                time.sleep(0.1)
        else:
            raise TimeoutError('owned native MinIO readiness deadline')
        self.identity = self.read_identity()
        self.command(['mb', 'owned/awaken-blobs'])
        self.command(['admin', 'user', 'add', 'owned', self.user, self.old_secret])
        self.command(['admin', 'policy', 'create', 'owned', 'awaken-fixture', str(self.policy)])
        self.command(['admin', 'policy', 'attach', 'owned', 'awaken-fixture', '--user', self.user])
        return self.receipt()

    def test_environment(self) -> dict[str, str]:
        values = {'AWAKEN_TEST_REQUIRE_S3': '1', 'AWAKEN_TEST_S3_ENDPOINT': self.endpoint, 'AWAKEN_TEST_S3_ACCESS_KEY': self.user,
                  'AWAKEN_TEST_S3_SECRET_KEY': self.new_secret if self.rotated else self.old_secret,
                  'AWAKEN_TEST_S3_ROTATION_ID': self.owner}
        if self.rotated:
            values['AWAKEN_TEST_S3_OLD_SECRET_KEY'] = self.old_secret
        return values

    def rotate(self) -> None:
        if self.rotated:
            raise RuntimeError('native MinIO credential transition was already applied')
        self.verify()
        self.command(['admin', 'user', 'remove', 'owned', self.user])
        self.command(['admin', 'user', 'add', 'owned', self.user, self.new_secret])
        self.command(['admin', 'policy', 'attach', 'owned', 'awaken-fixture', '--user', self.user])
        self.rotated = True
        self.verify()

    def receipt(self) -> dict:
        return {'kind': 'native-windows-process', 'binaries': self.binaries, 'pid': self.process.pid,
                'owner': self.owner, 'identity': self.identity, 'endpoint': self.endpoint,
                'authentication': 'per-run root and least-privilege application credentials; not recorded',
                'rotation_applied': self.rotated}

    def verify(self) -> None:
        if self.process is None or self.process.poll() is not None:
            raise RuntimeError('owned native MinIO process is no longer running')
        if self.read_identity() != self.identity:
            raise RuntimeError('owned native MinIO identity changed')

    def close(self) -> dict:
        try:
            if self.process is None:
                return {'stopped': True, 'not_started': True}
            if self.process.poll() is None:
                self.process.terminate()
            self.process.wait(timeout=10)
            return {'stopped': True, 'pid': self.process.pid, 'exit_code': self.process.returncode,
                    'method': 'retained-owned-process-handle'}
        finally:
            if self.log is not None:
                self.log.close()
