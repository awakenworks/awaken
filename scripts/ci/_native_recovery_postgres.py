"""Controlled native PostgreSQL cluster transitions for the recovery drill."""
from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import time

from _native_postgres import PostgresService
from _native_recovery_wal import sql_literal


def launch_existing(service: PostgresService, log_name: str, *, recovery: bool) -> dict:
    if service.process is not None and service.process.poll() is None:
        raise RuntimeError('owned cluster must stop before restarting')
    service.log = (service.output / log_name).open('ab')
    arguments = [service.binaries['postgres']['path'], '-D', str(service.data),
        '-h', '127.0.0.1', '-p', str(service.port), '-c', 'cluster_name=' + service.owner,
        '-c', 'shared_buffers=32MB', '-c', 'max_wal_size=128MB']
    if recovery:
        arguments += ['-c', 'archive_mode=off']
    service.process = subprocess.Popen(arguments, env=service.environment, stdout=service.log,
        stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, creationflags=subprocess.CREATE_NO_WINDOW)
    until = time.monotonic() + 60
    while time.monotonic() < until:
        if service.process.poll() is not None:
            raise RuntimeError('owned native PostgreSQL exited before readiness')
        try:
            identity = service.read_identity()
            service.assert_owned_pid()
            if not recovery or service.sql('SELECT pg_is_in_recovery()') == 'f':
                service.identity = identity
                return service.receipt()
        except (RuntimeError, subprocess.TimeoutExpired):
            pass
        time.sleep(0.1)
    raise TimeoutError('native PostgreSQL recovery/readiness deadline')


class ArchivePrimary(PostgresService):
    def __init__(self, binaries: Path, root: Path, output: Path, owner: str, archive: Path):
        super().__init__(binaries, root, output, owner)
        self.archive = archive.resolve(strict=True)

    def start(self) -> dict:
        super().start()
        self.verify()
        self.close()
        with (self.data / 'postgresql.auto.conf').open('a', encoding='utf-8') as stream:
            for key, value in (('archive_mode', 'on'), ('archive_library', 'basic_archive'),
                               ('basic_archive.archive_directory', self.archive.as_posix())):
                stream.write('\n' + key + '=' + sql_literal(value) + '\n')
        receipt = launch_existing(self, 'archive-postgres.log', recovery=False)
        if self.sql('SHOW archive_mode') != 'on' or self.sql('SHOW archive_library') != 'basic_archive':
            raise RuntimeError('native PostgreSQL WAL archive was not enabled')
        self.environment['PGDATABASE'] = 'awaken_ci'
        return receipt


class RestoredCluster(PostgresService):
    def __init__(self, primary: ArchivePrimary, root: Path, output: Path, owner: str,
                 restore_root: Path, target_lsn: str):
        import re
        if not re.fullmatch(r'[0-9A-F]+/[0-9A-F]+', target_lsn):
            raise ValueError('invalid recovery target LSN')
        restore_root = restore_root.resolve(strict=True)
        data, archive = restore_root / 'base', restore_root / 'wal'
        if (data.is_symlink() or archive.is_symlink() or not data.is_dir() or not archive.is_dir()
                or data.resolve() == primary.data.resolve()
                or data.resolve().parent != restore_root or archive.resolve().parent != restore_root):
            raise ValueError('recovery requires a separate owned backup data/archive pair')
        binaries = Path(primary.binaries['postgres']['path']).parent
        super().__init__(binaries, root, output, owner)
        # Adopt the backup credential and coordinate together, before a process
        # can be launched or an endpoint exposed to callers.
        self.data, self.password = data, primary.password
        self.url = f'postgres://postgres:{self.password}@127.0.0.1:{self.port}/awaken_ci'
        self.environment.update(PGPASSWORD=self.password, PGDATABASE='awaken_ci')
        helper = Path(__file__).with_name('_native_recovery_wal.py')
        command = ' '.join('"' + value.replace('\\', '/') + '"' for value in
            (sys.executable, str(helper), '--archive', str(archive), '--data', str(data), '%f', '%p'))
        with (data / 'postgresql.auto.conf').open('a', encoding='utf-8') as stream:
            stream.write('\nrestore_command=' + sql_literal(command) + '\nrecovery_target_lsn=' + sql_literal(target_lsn)
                         + "\nrecovery_target_action='promote'\n")
        (data / 'recovery.signal').touch(exist_ok=False)

    def start(self) -> dict:
        return launch_existing(self, 'restored-postgres.log', recovery=True)
