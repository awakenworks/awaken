#!/usr/bin/env python3
"""Export Docker images for the shared offline k3d importer.

Tagged dependencies keep platform filtering. An exact digest instead preserves
its OCI root: filtering an index changes identity, and Docker's digest-only save
omits the name containerd needs. Only the outer transport index is annotated;
the content-addressed root is checked and never rewritten.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile


def canonical_digest(reference):
    match = re.fullmatch(r'([^\s@]+)@(sha256:[0-9a-f]{64})', reference)
    if not match:
        raise ValueError("expected an exact image digest")
    repository, digest = match.groups()
    first = repository.split('/')[0]
    if '/' not in repository:
        repository = 'docker.io/library/' + repository
    elif '.' not in first and ':' not in first and first != 'localhost':
        repository = 'docker.io/' + repository
    elif repository.startswith('docker.io/') and repository.count('/') == 1:
        repository = 'docker.io/library/' + repository.removeprefix('docker.io/')
    return repository + '@' + digest, digest


def retain_digest_name(source, destination, reference):
    canonical, digest = canonical_digest(reference)
    root_blob = 'blobs/sha256/' + digest.removeprefix('sha256:')
    seen_index = seen_root = False
    with tarfile.open(fileobj=source, mode='r|') as incoming, tarfile.open(
        fileobj=destination, mode='w|', format=tarfile.PAX_FORMAT,
    ) as outgoing:
        for member in incoming:
            content = incoming.extractfile(member) if member.isfile() else None
            if member.name == 'index.json':
                if seen_index or content is None:
                    raise ValueError("ambiguous OCI transport index")
                seen_index = True
                index = json.load(content)
                roots = index.get('manifests', [])
                if len(roots) != 1 or roots[0].get('digest') != digest:
                    raise ValueError("export changed the requested image digest")
                roots[0].setdefault('annotations', {})['io.containerd.image.name'] = canonical
                encoded = json.dumps(index, separators=(',', ':')).encode()
                member.size = len(encoded)
                content = io.BytesIO(encoded)
            elif member.name == root_blob:
                if seen_root or content is None:
                    raise ValueError("ambiguous OCI root content")
                seen_root = True
                encoded = content.read()
                if 'sha256:' + hashlib.sha256(encoded).hexdigest() != digest:
                    raise ValueError("OCI root bytes do not match requested digest")
                content = io.BytesIO(encoded)
            outgoing.addfile(member, content)
    if not seen_index or not seen_root:
        raise ValueError("digest import requires a complete OCI root from Docker's containerd store")


def export_image(reference, platform, output):
    process = None
    try:
        if '@' not in reference:
            subprocess.run(['docker', 'save', '--platform', platform, '-o', str(output), reference], check=True)
            return
        canonical_digest(reference)  # Reject malformed input before Docker.
        process = subprocess.Popen(['docker', 'save', reference], stdout=subprocess.PIPE)
        with output.open('wb') as destination:
            retain_digest_name(process.stdout, destination, reference)
        process.stdout.close()
        status = process.wait()
        if status:
            raise subprocess.CalledProcessError(status, process.args)
    except BaseException:
        if process is not None:
            process.stdout.close()
            if process.poll() is None:
                process.kill()
            process.wait()
        output.unlink(missing_ok=True)
        raise


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--platform', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('reference')
    args = parser.parse_args()
    export_image(args.reference, args.platform, args.output)
