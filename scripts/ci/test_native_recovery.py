"""Fault tables for native physical recovery admission and restored values."""
from pathlib import Path
import hashlib
import io
import json
import tarfile
import tempfile
import unittest

import _native_recovery_wal as wal
import _native_recovery_bundle as bundle

class RecoveryAdmissionTests(unittest.TestCase):
    def test_manifest_rejects_missing_extra_and_mismatched_coordinates(self):
        digest=hashlib.sha256(b'content').hexdigest()
        good=dict(lsn='A/12AB',sha256=digest,key='reliability-recovery/sha256/'+digest)
        self.assertEqual(bundle.validate_manifest(good),good)
        cases=[None,{},dict(good,extra=True)]
        for key,values in {'lsn':['','A/12ab',"A/1'bad",None], 'sha256':['','A'*64,None], 'key':['foreign','',None]}.items():
            cases += [dict(good,**{key:value}) for value in values]
        for value in cases:
            with self.subTest(value=value),self.assertRaises(ValueError):bundle.validate_manifest(value)

    def test_wal_copy_requires_valid_name_before_any_filesystem_access(self):
        for name in ('','../escape','..\\escape','a'*24,'0'*23,'0'*24+':ads','A'*24+'/child'):
            with self.subTest(name=name),self.assertRaises(ValueError):
                wal.copy_archived_wal(Path('absent'),Path('absent'),name,Path('outside'))

    def test_wal_copy_distinguishes_missing_archive_and_exact_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);archive=root/'wal';archive.mkdir();data=root/'data';(data/'pg_wal').mkdir(parents=True)
            target=data/'pg_wal/RECOVERYXLOG';name='A'*24
            self.assertFalse(wal.copy_archived_wal(archive,data,name,target));self.assertFalse(target.exists())
            (archive/name).write_bytes(b'exact archived WAL')
            self.assertTrue(wal.copy_archived_wal(archive,data,name,target));self.assertEqual(target.read_bytes(),b'exact archived WAL')

    def test_wal_copy_cannot_overwrite_files_outside_the_cluster_wal_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);archive=root/'wal';archive.mkdir();data=root/'data';(data/'pg_wal').mkdir(parents=True)
            name='B'*24;(archive/name).write_bytes(b'archive');outside=root/'precious';outside.write_bytes(b'preserve')
            for target in (outside,data/'pg_wal/../PG_VERSION',data/'pg_wal/../../precious'):
                with self.subTest(target=str(target)),self.assertRaises(ValueError):wal.copy_archived_wal(archive,data,name,target)
            self.assertEqual(outside.read_bytes(),b'preserve')

    def make_bundle(self,root,*,bad=None):
        digest=hashlib.sha256(b'content').hexdigest();manifest=dict(lsn='A/1',sha256=digest,key='reliability-recovery/sha256/'+digest)
        path=root/'backup.tar'
        with tarfile.open(path,'w') as stream:
            for name,payload in [('base/PG_VERSION',b'16'),('wal/'+'A'*24,b'wal'),('backup-manifest.json',json.dumps(manifest).encode())]:
                member=tarfile.TarInfo(name);member.size=len(payload);stream.addfile(member,io.BytesIO(payload))
            if bad:
                member=tarfile.TarInfo(bad)
                if bad=='base/link':member.type=tarfile.SYMTYPE;member.linkname='../../escape'
                stream.addfile(member,io.BytesIO())
        return path,bundle.sha256(path),manifest

    def test_valid_bundle_round_trip_preserves_manifest_and_data(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);path,digest,manifest=self.make_bundle(root)
            bundle.unpack_bundle(path,digest,root/'restored',manifest)
            self.assertEqual((root/'restored/base/PG_VERSION').read_bytes(),b'16')
            self.assertEqual(json.loads((root/'restored/backup-manifest.json').read_text()),manifest)

    def test_corrupt_bundle_fails_before_extraction(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);path,digest,manifest=self.make_bundle(root);path.write_bytes(path.read_bytes()+b'changed')
            with self.assertRaises(RuntimeError):bundle.unpack_bundle(path,digest,root/'restored',manifest)
            self.assertFalse((root/'restored').exists())

    def test_unsafe_duplicate_and_link_members_fail_before_extraction(self):
        for name in ('../escape','/absolute','base/../escape','base/file:ads','foreign/file','base\\escape','base/link','base/PG_VERSION'):
            with self.subTest(name=name),tempfile.TemporaryDirectory() as directory:
                root=Path(directory);path,digest,manifest=self.make_bundle(root,bad=name)
                with self.assertRaises(ValueError):bundle.unpack_bundle(path,digest,root/'restored',manifest)
                self.assertFalse((root/'restored').exists())

    def test_every_recovery_value_is_required_independently(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);content=root/'content';content.write_bytes(b'content');digest=bundle.sha256(content)
            manifest=dict(lsn='A/1',sha256=digest,key='reliability-recovery/sha256/'+digest)
            good=dict(rows='at-target,base',primary_rows='3',row_coordinate=manifest['key']+'|'+digest,
                      manifest=manifest,restored_content=content,leaked_writes='0')
            bundle.verify_recovery_values(**good)
            for key,values in {'rows':['base','at-target','after-target,at-target,base'], 'primary_rows':['2','4'],
                               'row_coordinate':['foreign|'+digest,manifest['key']+'|wrong'], 'leaked_writes':['1']}.items():
                for value in values:
                    with self.subTest(key=key,value=value),self.assertRaises(RuntimeError):
                        bundle.verify_recovery_values(**dict(good,**{key:value}))
            content.write_bytes(b'corrupt')
            with self.assertRaises(RuntimeError):bundle.verify_recovery_values(**good)

class RecoveryArchiveIntervalTests(unittest.TestCase):
    def fixture(self,root,start=2,log=0):
        backup=root/'base';backup.mkdir();archive=root/'wal';archive.mkdir()
        name=f'00000001{log:08X}{start:08X}'
        lsn=(log*256+start)*bundle.WAL_BYTES+40
        label=f'START WAL LOCATION: {lsn >> 32:X}/{lsn & 0xFFFFFFFF:X} (file {name})\nSTART TIMELINE: 1\n'
        (backup/'backup_label').write_text(label,encoding='utf-8')
        return backup,archive

    def member(self,archive,name):
        with (archive/name).open('wb') as stream:stream.truncate(bundle.WAL_BYTES)

    def test_only_the_complete_backup_interval_enters_the_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            backup,archive=self.fixture(Path(directory))
            names=[f'0000000100000000{index:08X}' for index in range(1,6)]
            for name in names:self.member(archive,name)
            (archive/'00000002.history').write_bytes(b'unrelated')
            selected=bundle.select_wal_for_backup(backup,archive,names[3])
            self.assertEqual([path.name for path in selected],names[1:4])

    def test_log_rollover_is_a_contiguous_wal_interval(self):
        with tempfile.TemporaryDirectory() as directory:
            backup,archive=self.fixture(Path(directory),start=255)
            names=['0000000100000000000000FF','000000010000000100000000','000000010000000100000001']
            for name in names:self.member(archive,name)
            self.assertEqual([path.name for path in bundle.select_wal_for_backup(backup,archive,names[-1])],names)

    def test_missing_and_incomplete_segments_cannot_produce_a_partial_bundle(self):
        for incomplete in (False,True):
            with self.subTest(incomplete=incomplete),tempfile.TemporaryDirectory() as directory:
                backup,archive=self.fixture(Path(directory))
                self.member(archive,'000000010000000000000002');self.member(archive,'000000010000000000000004')
                if incomplete:(archive/'000000010000000000000003').write_bytes(b'partial')
                with self.assertRaisesRegex(ValueError,'complete segment'):
                    bundle.select_wal_for_backup(backup,archive,'000000010000000000000004')

    def test_malformed_reversed_cross_timeline_and_excessive_intervals_fail_early(self):
        with tempfile.TemporaryDirectory() as directory:
            backup,archive=self.fixture(Path(directory))
            for last in ('bad','000000010000000000000100','000000020000000000000004','000000010000000000000001','000000010000000000000042'):
                with self.subTest(last=last),self.assertRaises(ValueError):bundle.select_wal_for_backup(backup,archive,last)
            path=backup/'backup_label';original=path.read_text()
            for changed in ('',original+original,original.replace('START TIMELINE: 1','START TIMELINE: 2'),original.replace('0/2000028','0/3000028')):
                path.write_text(changed,encoding='utf-8')
                with self.subTest(label=changed),self.assertRaises(ValueError):bundle.select_wal_for_backup(backup,archive,'000000010000000000000004')
            self.assertEqual(list(archive.iterdir()),[])

import argparse
import contextlib
import copy
import os
from unittest import mock
import check_native_services as gate

INPUT_GLOBS = (
    "scripts/ci/**", ".github/workflows/windows-native-services.yml",
    # The work-tier selection tests it imports read the acceptance ledger.
    "docs/design/session-execution-test-acceptance/**",
    "crates/platform/environment/awaken-work-store/tests/work_queue_conformance/**",
)


class RecoveryControllerTests(unittest.TestCase):
    def test_invalid_recovery_capabilities_fail_before_any_process_launch(self):
        cases=[['--service','nats','--nats-bin','unused'],
               ['--service','s3','--minio-bin','unused','--mc-bin','unused'],
               ['--service','postgres','--postgres-bin','unused'],
               ['--service','postgres','--postgres-bin','unused','--minio-bin','unused'],
               ['--service','postgres','--postgres-bin','unused','--mc-bin','unused']]
        for arguments in cases:
            with self.subTest(arguments=arguments),mock.patch('deadline.run') as launch,contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as error:gate.main([*arguments,'--recovery','--output','unopened'])
                self.assertEqual(error.exception.code,2);launch.assert_not_called()

    @unittest.skipUnless(os.name=='nt','native Windows service admission')
    def test_recovery_intent_is_explicit_when_the_worker_is_constructed(self):
        base=['--service','postgres','--postgres-bin','unused','--output','unopened','--owned-worker']
        for enabled in (False,True):
            with self.subTest(enabled=enabled),mock.patch.object(gate,'worker',return_value=17) as worker:
                args=base+(['--recovery','--minio-bin','unused','--mc-bin','unused'] if enabled else [])
                self.assertEqual(gate.main(args),17)
                self.assertIs(worker.call_args.args[0].recovery,enabled)

    def test_recovery_primary_object_and_restore_failures_cannot_become_success(self):
        import rust_test_plan
        for fault in (None,'primary-start','object-start','domain','recovery','object-cleanup','primary-cleanup','domain-and-cleanup'):
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as directory:
                base=Path(directory);fixture=base/'fixture';fixture.mkdir()
                args=gate.parser().parse_args(['--service','postgres','--recovery','--output',str(base/'out'),
                    '--postgres-bin',str(base/'pg'),'--minio-bin',str(base/'minio.exe'),'--mc-bin',str(base/'mc.exe')])
                snapshots=[]
                def save_record(_output,value):snapshots.append(copy.deepcopy(value))
                primary,objects=mock.Mock(),mock.Mock()
                primary.url='postgres://fixture@127.0.0.1/owned'
                primary.start.return_value={'owner':'one'};objects.start.return_value={'owner':'one'}
                primary.close.return_value={'stopped':True};objects.close.return_value={'stopped':True}
                if fault=='primary-start':primary.start.side_effect=RuntimeError('primary start failed')
                if fault=='object-start':objects.start.side_effect=RuntimeError('object start failed')
                if fault in ('object-cleanup','domain-and-cleanup'):objects.close.side_effect=RuntimeError('object cleanup failed')
                if fault=='primary-cleanup':primary.close.side_effect=RuntimeError('primary cleanup failed')
                facts={'revision':b'revision','index_tree':b'tree','patch':b'patch'}
                code=17 if fault in ('domain','domain-and-cleanup') else 0
                with mock.patch.object(rust_test_plan,'working_subject',return_value=('source',facts)),\
                     mock.patch.object(gate.tempfile,'mkdtemp',return_value=str(fixture)),\
                     mock.patch.object(gate,'ArchivePrimary',return_value=primary),\
                     mock.patch.object(gate,'MinioService',return_value=objects),\
                     mock.patch.object(gate.subprocess,'run',return_value=mock.Mock(returncode=code)),\
                     mock.patch.object(gate,'reconcile_postgres',return_value=[{'verified':True}]),\
                     mock.patch.object(gate,'_save_record',side_effect=save_record),\
                     mock.patch.object(gate,'run_recovery',return_value={'status':'passed','cleanup':{'stopped':True}}) as recovery:
                    if fault=='recovery':recovery.side_effect=RuntimeError('restore failed')
                    status=gate.worker(args)
                value=snapshots[-1]
                self.assertEqual(status==0,fault is None)
                self.assertEqual(value['status']=='passed',fault is None)
                if fault in ('domain','domain-and-cleanup'):self.assertEqual(status,17)
                primary.close.assert_called_once();objects.close.assert_called_once()
                if fault in ('primary-start','object-start','domain','domain-and-cleanup'):recovery.assert_not_called()
                else:self.assertEqual(recovery.call_count,1,value)

    @unittest.skipUnless(os.name=='nt','native Windows service admission')
    def test_outer_success_requires_every_typed_recovery_and_cleanup_witness(self):
        good=dict(status='passed',cleanup={'stopped':True},recovery_required=True,
            recovery={'status':'passed','cleanup':{'stopped':True}},recovery_object_cleanup={'stopped':True})
        rows=[good]
        for key,changes in {'cleanup':[{}, {'stopped':'true'}, {'stopped':False}],
                            'recovery_required':[False,'1',None],
                            'recovery':[{}, {'status':'failed','cleanup':{'stopped':True}}, {'status':'passed','cleanup':{'stopped':'true'}}],
                            'recovery_object_cleanup':[{}, {'stopped':False}, {'stopped':'true'}]}.items():
            rows += [dict(good,**{key:value}) for value in changes]
        for index,value in enumerate(rows):
            with self.subTest(index=index),tempfile.TemporaryDirectory() as directory:
                output=Path(directory);(output/'result.json').write_text(json.dumps(value),encoding='utf-8')
                args=['--service','postgres','--postgres-bin','unused','--minio-bin','unused','--mc-bin','unused','--recovery','--output',str(output)]
                with mock.patch('deadline.run',return_value=0):status=gate.main(args)
                self.assertEqual(status,0 if index==0 else 1)

if __name__=='__main__':unittest.main(verbosity=2)
