"""Native formal fixtures must isolate writes without symlink privileges."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import formal_gate_mutations as fixtures
import formal_sources

# The private Git trees are generated fixtures; repository inputs are the CI
# Python helpers imported by this test and the checker that invokes it.
INPUT_GLOBS: tuple[str, ...] = ("scripts/ci/test_formal_gate_mutations.py",)


@unittest.skipUnless(os.name == "nt", "native Windows fixture admission")
class NativeFormalFixtureTests(unittest.TestCase):
    def test_tracked_copy_is_exact_private_and_restorable_without_symlinks(self):
        with tempfile.TemporaryDirectory(prefix="formal-native-copy-") as temporary:
            root = Path(temporary)
            source, case = root / "source", root / "case"
            source.mkdir()
            documents = {
                "scripts/ci/check_formal.sh": b"#!/bin/sh\necho baseline\n",
                "formal/obligations/example.toml": b'id = "original"\n',
                "crates/example/src/lib.rs": b"// original\r\npub fn sample() {}\r\n",
                "e2e/fixture space.bin": b"\x00\xfforiginal\r\n",
            }
            for relative, content in documents.items():
                path = source / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(content)
            env = formal_sources.detached_git_environment()
            subprocess.run(["git", "init", "--quiet", str(source)], env=env, check=True, capture_output=True)
            subprocess.run(["git", "-c", "core.autocrlf=false", "-C", str(source), "add", "--", *documents],
                           env=env, check=True, capture_output=True)
            (source / "untracked-cache.bin").write_bytes(b"not source")
            with patch.object(fixtures, "ROOT", source), patch.dict(fixtures.PRIVATE_ORIGINALS, clear=True), \
                    patch.object(fixtures.os, "symlink", side_effect=AssertionError("privileged link requested")):
                fixtures.prepare_case(case)
                self.assertEqual({p.relative_to(case).as_posix(): p.read_bytes()
                                  for p in case.rglob("*") if p.is_file()}, documents)
                self.assertFalse((case / ".git").exists())
                self.assertFalse((case / "untracked-cache.bin").exists())
                relative = "crates/example/src/lib.rs"
                private = fixtures.private_source(case, relative)
                private.write_bytes(b"first mutant")
                fixtures.private_source(case, relative).write_bytes(b"second mutant")
                self.assertEqual((source / relative).read_bytes(), documents[relative])
                self.assertEqual(fixtures.PRIVATE_ORIGINALS[private], documents[relative])
                for path, content in fixtures.PRIVATE_ORIGINALS.items():
                    path.write_bytes(content)
                self.assertEqual(private.read_bytes(), documents[relative])
                self.assertEqual({p: (source / p).read_bytes() for p in documents}, documents)

    def test_private_source_rejects_relative_and_absolute_escape_before_writing(self):
        with tempfile.TemporaryDirectory(prefix="formal-native-escape-") as temporary:
            root = Path(temporary)
            case = root / "case"
            case.mkdir()
            outside = root / "outside.rs"
            outside.write_bytes(b"original outside")
            with patch.dict(fixtures.PRIVATE_ORIGINALS, clear=True):
                for relative in ("../outside.rs", str(outside)):
                    with self.subTest(relative=relative), self.assertRaisesRegex(SystemExit, "could not privatize"):
                        fixtures.private_source(case, relative)
                    self.assertEqual(outside.read_bytes(), b"original outside")
                    self.assertEqual(fixtures.PRIVATE_ORIGINALS, {})


if __name__ == "__main__":
    unittest.main()
