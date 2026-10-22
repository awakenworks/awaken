"""Physical target-LSN recovery using caller-owned native PostgreSQL and S3."""
from __future__ import annotations

import json
from pathlib import Path
import re
import tarfile
import time

from _native_recovery_bundle import require, select_wal_for_backup, unpack_bundle, verify_recovery_values
from _native_recovery_postgres import ArchivePrimary, RestoredCluster
from _native_recovery_wal import sql_literal
from native_services import sha256, windows_executable


def run_recovery(primary: ArchivePrimary, objects, root: Path, output: Path) -> dict:
    root.mkdir()
    receipt=output/'recovery.json'
    # This packet records its declared platform; it grants no authority
    # over validation on other hosts or publication.
    record=dict(status='running',platform='Windows/MSVC',
        full_gate=False,scope='Physical recovery to a committed target LSN through real S3; same primary as the preceding domain tests.',
        primary_owner=primary.owner,object_owner=objects.owner,stages=[])
    def save():
        pending=receipt.with_suffix('.pending');pending.write_text(json.dumps(record,indent=2),encoding='utf-8');pending.replace(receipt)
    def stage(name):record['stages'].append(name);save()
    restored=None;failure=None;save()
    try:
        primary.verify();objects.verify()
        content=root/'content-before-target.txt';content.write_bytes(b'awaken immutable native recovery content\n')
        digest=sha256(content);key='reliability-recovery/sha256/'+digest
        objects.command(['mb','owned/awaken-blobs-restored'])
        objects.command(['cp',str(content),'owned/awaken-blobs/'+key])
        objects.command(['cp','owned/awaken-blobs/'+key,'owned/awaken-blobs-restored/'+key])
        primary.sql('CREATE TABLE awaken_native_recovery_probe(operation_id text PRIMARY KEY, object_key text NOT NULL, digest text NOT NULL)')
        def insert(label):
            primary.sql('INSERT INTO awaken_native_recovery_probe VALUES ('+','.join(sql_literal(value) for value in (label,key,digest))+')')
        insert('base');stage('immutable-object-copy-and-base-operation')
        binaries=Path(primary.binaries['postgres']['path']).parent
        for name in ('pg_basebackup','pg_verifybackup'):
            primary.binaries[name]=windows_executable(binaries/(name+'.exe'))
        backup=root/'base'
        primary.command('pg_basebackup',['-D',str(backup),'-X','none','--checkpoint=fast','--no-password'],timeout=120)
        insert('at-target');lsn=primary.sql('SELECT pg_current_wal_lsn()')
        require(re.fullmatch(r'[0-9A-F]+/[0-9A-F]+',lsn) is not None,'invalid recovery target LSN')
        def archive_segment():
            segment=primary.sql('SELECT pg_walfile_name(pg_switch_wal())')
            require(re.fullmatch(r'[0-9A-F]{24}',segment) is not None,'invalid recovery WAL segment')
            until=time.monotonic()+45
            while time.monotonic()<until:
                primary.verify()
                path=primary.archive/segment
                if path.is_file() and path.stat().st_size==16*1024*1024:return segment
                time.sleep(0.1)
            raise TimeoutError('owned WAL archive did not become complete')
        target_segment=archive_segment();insert('after-target');after_segment=archive_segment()
        primary.command('pg_verifybackup',['--exit-on-error','--wal-directory',str(primary.archive),str(backup)],timeout=60)
        stage('verified-base-backup-and-two-committed-wal-segments')
        manifest=dict(lsn=lsn,key=key,sha256=digest)
        (root/'backup-manifest.json').write_text(json.dumps(manifest),encoding='utf-8')
        bundle=root/'backup.tar'
        wal_members=select_wal_for_backup(backup,primary.archive,after_segment)
        with tarfile.open(bundle,'w') as stream:
            stream.add(backup,arcname='base')
            directory=tarfile.TarInfo('wal');directory.type=tarfile.DIRTYPE;stream.addfile(directory)
            for member in wal_members:stream.add(member,arcname='wal/'+member.name)
            stream.add(root/'backup-manifest.json',arcname='backup-manifest.json')
        bundle_digest=sha256(bundle)
        objects.command(['mb','owned/awaken-recovery'])
        objects.command(['cp',str(bundle),'owned/awaken-recovery/backup.tar'])
        downloaded=root/'downloaded.tar'
        objects.command(['cp','owned/awaken-recovery/backup.tar',str(downloaded)])
        restore_root=root/'restore'
        unpack_bundle(downloaded,bundle_digest,restore_root,manifest)
        primary.command('pg_verifybackup',['--exit-on-error','--wal-directory',str(restore_root/'wal'),str(restore_root/'base')],timeout=60)
        stage('s3-bundle-transfer-safe-extraction-and-offline-backup-verification')
        restored=RestoredCluster(primary,root/'restore-control',output,primary.owner+'-restore',restore_root,lsn)
        record['restored']=restored.start();stage('native-target-lsn-promotion')
        rows=restored.sql("SELECT string_agg(operation_id,',' ORDER BY operation_id) FROM awaken_native_recovery_probe")
        primary_rows=primary.sql('SELECT count(*) FROM awaken_native_recovery_probe')
        coordinate=restored.sql("SELECT object_key||'|'||digest FROM awaken_native_recovery_probe WHERE operation_id='at-target'")
        restored_content=root/'restored-content.txt'
        objects.command(['cp','owned/awaken-blobs-restored/'+key,str(restored_content)])
        restored.sql("INSERT INTO awaken_native_recovery_probe VALUES ('restored-write','isolated','isolated')")
        leaked=primary.sql("SELECT count(*) FROM awaken_native_recovery_probe WHERE operation_id='restored-write'")
        verify_recovery_values(rows=rows,primary_rows=primary_rows,row_coordinate=coordinate,manifest=manifest,
                               restored_content=restored_content,leaked_writes=leaked)
        primary.verify();restored.verify();objects.verify()
        record.update(target_lsn=lsn,target_segment=target_segment,after_segment=after_segment,
            rows=rows,bundle_sha256=bundle_digest,content_sha256=digest,restored_write_isolated=True,
            wal_members=[path.name for path in wal_members],bundle_bytes=bundle.stat().st_size,
            offline_backup_verified=True)
        stage('exact-target-rows-object-digest-and-independent-write')
    except Exception as error:
        failure=error;record['error']=str(error)
    finally:
        try:
            record['cleanup']=restored.close() if restored is not None else dict(stopped=True,not_started=True)
        except Exception as error:
            failure=failure or error;record['cleanup_error']=str(error)
        record.update(status='passed' if failure is None else 'failed',exit_code=0 if failure is None else 1);save()
    if failure is not None:raise RuntimeError('native physical recovery failed; inspect recovery.json') from failure
    return record
