"""Cause/effect tests for the one upstream local PV deployment and installer."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "scripts/ci"))
import _strict_unittest as strict_unittest
from _native_shell import bash_executable

ROOT = Path(__file__).resolve().parent


class LocalStorageContract(unittest.TestCase):
    def test_upstream_identity_and_rendered_storage_boundary(self):
        # S1 exact upstream templates + reviewed values => one standard class,
        # controller and RBAC. S2 any template drift => fail provenance check.
        # The class never becomes default; any namespace can request it with
        # RWO+Filesystem+matching capacity, while normal scheduler topology stays.
        provenance = json.loads((ROOT / 'chart/UPSTREAM.json').read_text())
        for name, digest in provenance['files'].items():
            self.assertEqual(hashlib.sha256((ROOT / 'chart' / name).read_bytes()).hexdigest(), digest, name)
        rendered = subprocess.check_output(['helm', 'template', 'awaken-local-storage', str(ROOT / 'chart'),
            '--namespace', 'awaken-storage', '-f', str(ROOT / 'values.yaml')], text=True)
        self.assertEqual(rendered.count('kind: StorageClass\n'), 1)
        self.assertEqual(rendered.count('kind: DaemonSet\n'), 1)
        self.assertIn('provisioner: kubernetes.io/no-provisioner', rendered)
        self.assertIn('volumeBindingMode: WaitForFirstConsumer', rendered)
        self.assertNotIn('storageclass.kubernetes.io/is-default-class', rendered)
        self.assertIn('volumeMode: Filesystem', rendered)
        self.assertIn('accessMode: ReadWriteOnce', rendered)
        self.assertIn('reclaimPolicy: Delete', rendered)
        self.assertNotIn('kind: Job\n', rendered)
        self.assertNotIn('kind: PersistentVolumeClaim\n', rendered)
        self.assertIn('mountPropagation: HostToContainer', rendered)
        self.assertIn('awaken.dev/local-storage: "true"', rendered)
        self.assertIn('imagePullPolicy: IfNotPresent', rendered)
        image = next(line.strip().split('image: ', 1)[1] for line in rendered.splitlines() if line.strip().startswith('image: '))
        self.assertRegex(image, r'@sha256:[0-9a-f]{64}$')

    def test_installer_admits_only_absent_or_exact_owned_class(self):
        # Installer decision table I1 absent, I2 matching owned class => render,
        # apply and await controller; I3 foreign owner, I4 different binding mode,
        # I5 forbidden observation => reject before any mutation. Real installer
        # and Helm execute; only Kubernetes infrastructure responses are faked.
        existing = {'metadata': {'labels': {'app.kubernetes.io/instance': 'awaken-local-storage'}},
            'provisioner': 'kubernetes.io/no-provisioner', 'volumeBindingMode': 'WaitForFirstConsumer', 'reclaimPolicy': 'Delete'}
        for scenario in ('absent', 'owned', 'foreign', 'binding_conflict', 'forbidden'):
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary);calls = directory / 'calls'; binary = directory / 'kubectl'
                binary.write_text('''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
args=sys.argv[1:]
with Path(os.environ['CALLS']).open('a') as f:f.write(json.dumps(args)+'\\n')
assert args[:2]==['--context','fixture']
if args[2:4]==['get','storageclass']:
    scenario=os.environ['SCENARIO']
    if scenario=='forbidden':print('Forbidden',file=sys.stderr);raise SystemExit(1)
    if scenario!='absent':
        value=json.loads(os.environ['EXISTING'])
        if scenario=='foreign':value['metadata']['labels']={}
        if scenario=='binding_conflict':value['volumeBindingMode']='Immediate'
        print(json.dumps(value))
elif args[2]=='create':print('apiVersion: v1\\nkind: Namespace\\nmetadata:\\n  name: awaken-storage')
elif args[2]=='apply':sys.stdin.read() if args[-1]=='-' else None
elif args[2]=='-n':assert 'rollout' in args
else:raise AssertionError(args)
''');binary.chmod(0o755)
                result = subprocess.run([bash_executable(), (ROOT / 'install.sh').as_posix(), '--context', 'fixture', (directory / 'out').as_posix()],
                    env=dict(os.environ, PATH=str(directory)+os.pathsep+os.environ['PATH'], CALLS=str(calls),
                        SCENARIO=scenario, EXISTING=json.dumps(existing)), capture_output=True, text=True, timeout=15)
                self.assertEqual(result.returncode == 0, scenario in ('absent', 'owned'), result.stderr)
                requests = [json.loads(line) for line in calls.read_text().splitlines()]
                mutations = [args for args in requests if args[2] == 'apply']
                self.assertEqual(len(mutations), 2 if scenario in ('absent', 'owned') else 0)

    def test_unknown_invocation_has_no_effect(self):
        # I6 unknown or incomplete arguments => reject before Helm/Kubernetes.
        result = subprocess.run([bash_executable(), (ROOT / 'install.sh').as_posix(), '--unknown'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)


if __name__ == '__main__':
    strict_unittest.main()
