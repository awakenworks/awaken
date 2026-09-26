"""Cause/effect tests for the one Docker-to-containerd archive adapter."""

import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

from image_archive import canonical_digest, retain_digest_name


ROOT = b'{"schemaVersion":2,"manifests":[]}'
DIGEST = 'sha256:' + hashlib.sha256(ROOT).hexdigest()
BLOB = 'blobs/sha256/' + DIGEST.removeprefix('sha256:')
REFERENCE = 'sandbox@' + DIGEST


def fixture(mode='valid'):
    index = {'schemaVersion': 2, 'manifests': [{'digest': DIGEST, 'size': len(ROOT)}]}
    if mode == 'wrong-root':
        index['manifests'][0]['digest'] = 'sha256:' + '0' * 64
    if mode == 'duplicate-descriptor':
        index['manifests'].append(dict(index['manifests'][0]))
    if mode == 'provenance':
        index['manifests'].insert(0, {'digest': 'sha256:' + '1' * 64,
                                    'annotations': {'io.containerd.manifest.subject': DIGEST}})
    entries = [('index.json', json.dumps(index).encode()), (BLOB, ROOT), ('layer', b'unchanged')]
    if mode == 'missing-index': entries.pop(0)
    if mode == 'missing-root': entries.pop(1)
    if mode == 'duplicate-index': entries.append(entries[0])
    if mode == 'duplicate-root': entries.append(entries[1])
    if mode == 'corrupt-root': entries[1] = (BLOB, b'corrupt')
    if mode == 'malformed-index': entries[0] = ('index.json', b'{')
    result = io.BytesIO()
    with tarfile.open(fileobj=result, mode='w') as archive:
        for name, body in entries:
            member = tarfile.TarInfo(name)
            member.size = len(body)
            archive.addfile(member, io.BytesIO(body))
    return result.getvalue()


class ImageArchiveTests(unittest.TestCase):
    def test_exact_descriptor_is_selected_beside_provenance(self):
        # I13: multiple outer descriptors + exactly one requested digest =>
        # select that descriptor and canonical name, preserve root/layer bytes.
        # I14: multiple matching descriptors => reject ambiguous transport;
        # I2/I7 still reject absent requested digest or corrupted exact root.
        output = io.BytesIO()
        retain_digest_name(io.BytesIO(fixture('provenance')), output, REFERENCE)
        with tarfile.open(fileobj=io.BytesIO(output.getvalue())) as archive:
            self.assertEqual(archive.extractfile(BLOB).read(), ROOT)
            self.assertEqual(archive.extractfile('layer').read(), b'unchanged')
            descriptors = json.load(archive.extractfile('index.json'))['manifests']
            self.assertEqual(len(descriptors), 1)
            self.assertEqual(descriptors[0]['digest'], DIGEST)
            self.assertEqual(descriptors[0]['annotations']['io.containerd.image.name'],
                             'docker.io/library/' + REFERENCE)
        with self.assertRaisesRegex(ValueError, 'ambiguous OCI transport root'):
            retain_digest_name(io.BytesIO(fixture('duplicate-descriptor')), io.BytesIO(), REFERENCE)

    def test_exact_root_is_preserved_and_only_transport_name_changes(self):
        # I1: requested digest matches one OCI index entry and actual root bytes
        # => retain every content blob, add the canonical CRI reference only to
        # transport metadata. Test implicit/explicit registry and namespace names.
        for repository, expected in (
            ('sandbox', 'docker.io/library/sandbox'),
            ('team/sandbox', 'docker.io/team/sandbox'),
            ('docker.io/sandbox', 'docker.io/library/sandbox'),
            ('registry.test:5000/team/sandbox', 'registry.test:5000/team/sandbox'),
            ('localhost/sandbox', 'localhost/sandbox'),
        ):
            with self.subTest(repository=repository):
                output = io.BytesIO()
                retain_digest_name(io.BytesIO(fixture()), output, repository + '@' + DIGEST)
                with tarfile.open(fileobj=io.BytesIO(output.getvalue())) as archive:
                    self.assertEqual(archive.extractfile(BLOB).read(), ROOT)
                    self.assertEqual(archive.extractfile('layer').read(), b'unchanged')
                    index = json.load(archive.extractfile('index.json'))
                    self.assertEqual(index['manifests'][0]['digest'], DIGEST)
                    self.assertEqual(index['manifests'][0]['annotations']['io.containerd.image.name'], expected + '@' + DIGEST)

    def test_missing_ambiguous_changed_and_corrupt_evidence_is_rejected(self):
        # I2-I8: each false cause independently denies qualification: missing
        # index/root, duplicate metadata/content, rewritten digest, corrupt bytes,
        # malformed JSON. No config-ID or tag fallback can satisfy these rows.
        for mode, reason in (
            ('missing-index', 'complete OCI root'), ('missing-root', 'complete OCI root'),
            ('duplicate-index', 'ambiguous OCI transport'), ('duplicate-root', 'ambiguous OCI root'),
            ('wrong-root', 'changed the requested'), ('corrupt-root', 'bytes do not match'),
        ):
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, reason):
                retain_digest_name(io.BytesIO(fixture(mode)), io.BytesIO(), REFERENCE)
        with self.assertRaises(json.JSONDecodeError):
            retain_digest_name(io.BytesIO(fixture('malformed-index')), io.BytesIO(), REFERENCE)
        for reference in ('tag:latest', 'repo@sha256:short', '--bad@sha256:short'):
            with self.subTest(reference=reference), self.assertRaisesRegex(ValueError, 'exact image digest'):
                canonical_digest(reference)

    def test_real_export_process_preserves_status_and_removes_failed_output(self):
        # I9 tag => one platform-filtered save; I10 digest => one unfiltered save
        # and exact annotation; I11 valid bytes + Docker nonzero => fail + remove
        # incomplete archive; I12 malformed archive => same cleanup. This invokes
        # the real CLI and a fake Docker process, not an exporter reimplementation.
        with tempfile.TemporaryDirectory(prefix='awaken-image-archive-') as tmp:
            root = Path(tmp)
            binary = root / 'docker'
            binary.write_text(f'#!{sys.executable}\n' + '''import json, os, sys
from pathlib import Path
args = sys.argv[1:]
Path(os.environ['CALLS']).write_text(json.dumps(args))
data = Path(os.environ['ARCHIVE']).read_bytes()
if '-o' in args: Path(args[args.index('-o')+1]).write_bytes(data)
else: sys.stdout.buffer.write(data)
raise SystemExit(int(os.environ['STATUS']))
''')
            binary.chmod(0o755)
            source, output, calls = root / 'input.tar', root / 'output.tar', root / 'calls'
            for reference, status, mode in (('sandbox:local', 0, 'valid'), (REFERENCE, 0, 'valid'),
                                            (REFERENCE, 0, 'provenance'), (REFERENCE, 0, 'duplicate-descriptor'),
                                            (REFERENCE, 17, 'valid'), (REFERENCE, 0, 'wrong-root')):
                with self.subTest(reference=reference, status=status, mode=mode):
                    source.write_bytes(fixture(mode))
                    result = subprocess.run([
                        sys.executable, str(Path(__file__).with_name('image_archive.py')),
                        '--platform', 'linux/amd64', '--output', str(output), reference,
                    ], env=dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                                ARCHIVE=str(source), CALLS=str(calls), STATUS=str(status)),
                       capture_output=True, text=True, timeout=5)
                    success = status == 0 and mode in ('valid', 'provenance')
                    self.assertEqual(result.returncode == 0, success, result.stderr)
                    self.assertEqual(output.exists(), success)
                    arguments = json.loads(calls.read_text())
                    self.assertEqual('--platform' in arguments, '@' not in reference)
                    self.assertEqual(arguments[-1], reference)
                    if status: self.assertIn('exit status 17', result.stderr)


if __name__ == '__main__':
    unittest.main()
