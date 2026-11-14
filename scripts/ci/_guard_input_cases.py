"""Guard input boundary cases: formal_sources reads feeding the source guards.

check_secrets.py, check_file_limits.py and check_repository_hygiene.py all read
the bytes a commit judges through formal_sources.py; these cases drive those
guards' real mains over that boundary. They lived in formal_sources.py, which
made it import the three guards: generate_hooks.py --script-tests follows
imports, so every test reaching formal_sources ran on any edit to a guard.
check_secrets.py --self-test runs them.
"""

from __future__ import annotations

import os
from pathlib import Path
import subprocess

import formal_sources


def selftest_guard_inputs() -> None:
    """The guard input boundary's cause/effect cases, using actual Git and mains.

    Valid root + same-hook index -> selected index bytes; foreign hook ->
    root's own bytes; failed index/blob/tool -> original exception, never an
    empty success. Discovery may admit an explicit non-Git fixture only.
    SIO1 own alternate index / default working bytes; SIO2 foreign hook;
    SIO3 bad index/blob/HEAD/tool; SIO4 empty/nonrepo/unborn/absent HEAD path;
    SIO5 exact NUL path identity and newline decoding; SIO6 one hygiene read
    feeds both original text and executable matchers. All fixture mutations
    are detached from the invoking hook; its repository is never initialized.
    """
    import contextlib
    import io
    import tempfile
    from unittest.mock import patch

    import check_file_limits
    import check_repository_hygiene
    import check_secrets

    environment = formal_sources.detached_git_environment()
    owners = (check_secrets, check_file_limits, check_repository_hygiene)

    def git(root, *arguments):
        return subprocess.check_output(["git", *arguments], cwd=root, env=environment)

    def prepare(root):
        root.mkdir()
        (root / "docs").mkdir()
        (root / "probe.py").write_text("print(1)\n", encoding="utf-8")
        (root / "docs/probe.md").write_text("# short\n", encoding="utf-8")
        for arguments in (("init", "--quiet"), ("add", "."),
                          ("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                           "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "-m", "fixture")):
            git(root, *arguments)
        git(root, "config", "core.autocrlf", "false")

    def invoke(owner, root, arguments, hook):
        output, errors = io.StringIO(), io.StringIO()
        with patch.object(owner, "REPO_ROOT", root), patch.dict(os.environ, hook, clear=True), \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            return owner.main(arguments), output.getvalue(), errors.getvalue()

    with tempfile.TemporaryDirectory(prefix="awaken-guard-inputs-") as temporary:
        directory = Path(temporary)
        root, foreign = directory / "repo", directory / "foreign"
        prepare(root)
        prepare(foreign)
        (root / "probe.py").write_text('token = "' + 'abc12345' + 'SECRET"\n# bad\u200b\n', encoding="utf-8")
        (root / "docs/probe.md").write_text("# large\n" * 1201, encoding="utf-8")
        git(root, "add", ".")
        partial = root / ".git/partial-index"
        subprocess.check_output(["git", "read-tree", "HEAD"], cwd=root,
                                env={**environment, "GIT_INDEX_FILE": str(partial)})
        own = {**environment, "GIT_DIR": str(root / ".git"), "GIT_WORK_TREE": str(root),
               "GIT_INDEX_FILE": str(partial)}
        decoy = {**environment, "GIT_DIR": str(foreign / ".git"), "GIT_WORK_TREE": str(foreign),
                 "GIT_INDEX_FILE": str(foreign / ".git/index"), "GIT_PREFIX": "hook/"}
        protected = {path.relative_to(foreign).as_posix(): path.read_bytes()
                     for path in foreign.rglob("*") if path.is_file()}
        for owner in owners:
            # SIO1: a real own alternate index is clean while working bytes
            # are bad. Staged succeeds; default still scans the working file.
            clean = invoke(owner, root, ["--staged"], own)
            assert clean[0] == 0, "SIO1 own partial index"
            if owner is check_file_limits:
                assert "1201" not in clean[2], "SIO1 clean index has no working-file size warning"
            # SIO1/SIO2 decision table: line-only findings warn with status 0;
            # secrets/hygiene findings retain status 1. Both working and foreign
            # hook modes must still scan the ROOT's bad bytes. Requiring the
            # exact file/count warning catches the old false-empty status 0 even
            # though a line measurement no longer rejects the change.
            for arguments, hook, rule in (
                ([], own, "SIO1 default working bytes"),
                (["--staged"], decoy, "SIO2 foreign hook cannot hide debt"),
            ):
                status, _, diagnostic = invoke(owner, root, arguments, hook)
                if owner is check_file_limits:
                    assert status == 0, rule
                    assert "docs/probe.md" in diagnostic and "1201" in diagnostic, rule
                    assert "warn" in diagnostic.lower(), rule
                else:
                    assert status == 1, rule
        assert protected == {path.relative_to(foreign).as_posix(): path.read_bytes()
                             for path in foreign.rglob("*") if path.is_file()}, "SIO2 no foreign mutation"

        broken = root / ".git/broken-index"
        broken.write_bytes(b"not an index")
        for owner in owners:
            # SIO3: an actual corrupted index returns Git128; no guard may
            # translate it into its successful no-staged-files message.
            try:
                invoke(owner, root, ["--staged"], {**own, "GIT_INDEX_FILE": str(broken)})
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 preserve Git failure"
            else:
                raise AssertionError("SIO3 broken index became an empty PASS")
            # Invalid argv must be rejected before that same failing read.
            try:
                invoke(owner, root, ["--unknown"], {**own, "GIT_INDEX_FILE": str(broken)})
            except SystemExit as error:
                assert error.code == 2, "SIO3 parse before IO"
            else:
                raise AssertionError("SIO3 unknown argv admitted")

        # SIO5: spaces/Unicode/option-shaped names are portable. Raw undecodable
        # bytes and newline filenames are POSIX filesystem causes, not Windows
        # filenames. Keep every other input/error case active on both systems.
        names = ("space name.py", "nonascii-名.py", "-option.py")
        if os.name != "nt":
            names += ("line\nname.py", os.fsdecode(b"raw-\xff.py"))
        for name in names:
            (root / name).write_bytes(b"print(1)\r\nprint(2)\r")
        git(root, "add", "--", *names)
        with patch.dict(os.environ, environment, clear=True):
            listed = formal_sources.git_paths(root, "diff", "--cached", "--name-only", "-z", "--diff-filter=ACMR")
            assert set(names) <= set(listed), "SIO5 exact path identity"
            for name in names:
                assert formal_sources.file_text(root, name, True) == "print(1)\r\nprint(2)\r", "SIO5 staged raw decode"
                assert formal_sources.file_text(root, name, False) == "print(1)\nprint(2)\n", "SIO5 working newline decode"
            untracked = root / (os.fsdecode(b"untracked-\xff.py") if os.name != "nt" else "untracked-名.py")
            untracked.write_bytes(b"untracked\n")
            assert formal_sources.untracked_sources(root) == {untracked.name: b"untracked\n"}, "SIO5 one framing primitive preserves source-facts raw filename/bytes"
            assert formal_sources.committed_text(root, "absent.py") is None, "SIO4 genuinely absent HEAD path"
            try:
                formal_sources.file_bytes(root, "absent.py", True)
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 missing selected index blob is an error"
            else:
                raise AssertionError("SIO3 index blob failure became None")

        # SIO5 grep-record causes: the existing portable/raw/LF path set;
        # UTF-8 body, blank line and line number. Both public readers must
        # preserve the same exact staged path/content, never split a path at LF.
        # | Rule | path/record | Reader | Effect |
        # | G1 | real Git names + UTF-8/blank lines | source_text +/- containing | exact text |
        # | G2 | same real Git names | matching_lines | exact path, line, body |
        # | G3 | truncated/invalid protocol fields | both | SourceError |
        # | G4 | invalid UTF-8 text | both | original UnicodeDecodeError |
        grep_line = "# grep framing 名"
        grep_text = grep_line + "\n\nprint('line-3')\n"
        for name in names:
            (root / name).write_bytes(grep_text.encode("utf-8"))
        git(root, "add", "--", *names)
        with patch.dict(os.environ, environment, clear=True):
            for containing in ((), (grep_line,)):
                selected = formal_sources.source_text(root, "*.py", containing)
                assert {name: selected[name] for name in names} == {
                    name: grep_text for name in names
                }, "SIO5 G1 both source_text modes retain exact raw/LF path and UTF-8 body"
                if containing:
                    assert set(selected) == set(names), "SIO5 G1 containing keeps the exact selection"
            assert formal_sources.matching_lines(root, grep_line, lambda relative: relative in names) == sorted(
                (name, 1, grep_line) for name in names
            ), "SIO5 G2 exact matching_lines path/number/UTF-8 body"
            for wire, failure in [
                (b"unterminated path", formal_sources.SourceError),
                (b"source.py\0x\0line\n", formal_sources.SourceError),
                (b"source.py\0" b"0\0line\n", formal_sources.SourceError),
                (b"\0" b"1\0line\n", formal_sources.SourceError),
                (b"source.py\0" b"1\0unterminated", formal_sources.SourceError),
                (b"source.py\0" b"1\0\xff\n", UnicodeDecodeError),
            ]:
                with patch.dict(vars(formal_sources), _git_grep=lambda *_, data=wire: data):
                    for reader, arguments in [
                        (formal_sources.source_text, (root, "*.py")),
                        (formal_sources.matching_lines, (root, grep_line, lambda _: True)),
                    ]:
                        try:
                            reader(*arguments)
                        except failure:
                            pass
                        else:
                            raise AssertionError("SIO3 G3/G4 malformed grep record became source")

        reads = []
        original = formal_sources.file_bytes
        def observed(actual_root, relative, staged):
            reads.append(relative)
            return original(actual_root, relative, staged)
        with patch.dict(vars(formal_sources), file_bytes=observed):
            assert invoke(check_repository_hygiene, root, ["--staged"], environment)[0] == 1
        assert len(reads) == len(set(reads)) and "probe.py" in reads, "SIO6 one blob feeds both matchers"
        # SIO5/SIO6: read_text's universal-newline projection and raw bytes'
        # splitlines give identical line scopes to the actual Unicode, license
        # and anchored conflict matchers, including bare CR and CRLF.
        original_text = ("ok\nbad\u200b\n<<<<<<< HEAD\nPermission is hereby granted,"
                         + " free of charge\n")
        for newline in ("\n", "\r\n", "\r"):
            data = original_text.replace("\n", newline).encode()
            raw = check_repository_hygiene.check_text_content({"newline.py": data})
            universal = check_repository_hygiene.check_text_content({"newline.py": original_text.encode()})
            assert raw == universal and len(raw) == 3, "SIO5/SIO6 actual newline matcher scope"

        disk, empty = directory / "disk", directory / "empty"
        disk.mkdir()
        (disk / "fixture.py").write_text("fixture\n")
        empty.mkdir()
        git(empty, "init", "--quiet")
        (empty / "untracked.py").write_text("untracked\n")
        with patch.dict(os.environ, environment, clear=True):
            assert formal_sources.tracked_files(disk, allow_nonrepository=True) == ["fixture.py"], "SIO4 admitted disk fixture"
            assert formal_sources.tracked_files(empty, allow_nonrepository=True) == [], "SIO4 empty Git set is not disk fallback"
            assert formal_sources.committed_text(empty, "untracked.py") is None, "SIO4 unborn HEAD"
        (empty / "tests").mkdir()
        (empty / "tests/new.rs").write_text("fn test() {}\n")
        git(empty, "add", "tests/new.rs")
        assert invoke(check_file_limits, empty, ["--staged"], environment)[0] == 0, "SIO4 unborn short new source"
        with patch.dict(os.environ, {**environment, "PATH": str(disk)}, clear=True):
            try:
                formal_sources.tracked_files(root, allow_nonrepository=True)
            except OSError:
                pass
            else:
                raise AssertionError("SIO3 missing Git became a disk fallback")

        # SIO3/SIO4: a dangling .git link is an invalid repository marker,
        # never the explicitly admitted non-repository fixture case.
        # SIO3: the POSIX physical dangling-link case needs no privilege. NT
        # must not acquire symlink privileges for a Linux source regression;
        # malformed/index/foreign/HEAD cases above still run there. Windows
        # physical execution remains its own qualification, not this Linux run.
        if os.name != "nt":
            (disk / ".git").symlink_to(directory / "absent-git-directory", target_is_directory=True)
            with patch.dict(os.environ, environment, clear=True):
                try:
                    formal_sources.tracked_files(disk, allow_nonrepository=True)
                except (subprocess.CalledProcessError, formal_sources.SourceError) as error:
                    # Outside any checkout Git refuses with 128; a task-owned
                    # TMPDIR inside a checkout can discover the parent, which
                    # the exact-root admission rejects as SourceError. Both
                    # paths must refuse this dangling marker, never disk-walk.
                    if isinstance(error, subprocess.CalledProcessError):
                        assert error.returncode == 128, "SIO3 dangling repository marker"
                    else:
                        assert "not an admitted repository top" in str(error), "SIO3 parent-root refusal"
                else:
                    raise AssertionError("SIO3 dangling .git became a disk fallback")

        branch = git(root, "symbolic-ref", "HEAD").decode().strip()
        revision = git(root, "rev-parse", "HEAD").decode().strip()
        # SIO4: detached and packed valid HEADs still bind the same previous
        # bytes. Absence is established by Git, not by a loose-ref path check.
        git(root, "checkout", "--detach", "--quiet", revision)
        with patch.dict(os.environ, environment, clear=True):
            assert formal_sources.committed_text(root, "probe.py") == "print(1)\n", "SIO4 valid detached HEAD"
            assert formal_sources.committed_text(root, "absent.py") is None, "SIO4 absent path under detached HEAD"
        git(root, "symbolic-ref", "HEAD", branch)
        git(root, "pack-refs", "--all", "--prune")
        with patch.dict(os.environ, environment, clear=True):
            assert formal_sources.committed_text(root, "probe.py") == "print(1)\n", "SIO4 valid packed branch HEAD"
        record = root / ".git" / branch
        record.parent.mkdir(parents=True, exist_ok=True)
        # SIO3/SIO4: quiet rev-parse reports1 both for unborn and malformed
        # loose refs. Existing malformed metadata is a hard source error;
        # the earlier genuine-unborn case must still return None.
        record.write_text("not-an-oid\n")
        with patch.dict(os.environ, environment, clear=True):
            try:
                formal_sources.committed_text(root, "probe.py")
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 malformed existing HEAD"
            else:
                raise AssertionError("SIO3 malformed existing HEAD became unborn")
        record.write_text("a" * 40 + "\n")
        with patch.dict(os.environ, environment, clear=True):
            try:
                formal_sources.committed_text(root, "probe.py")
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 failed HEAD object"
            else:
                raise AssertionError("SIO3 unreadable HEAD became an absent historical file")
        record.unlink()
        (root / ".git/packed-refs").write_text("not-a-packed-ref\n")
        with patch.dict(os.environ, environment, clear=True):
            try:
                formal_sources.committed_text(root, "probe.py")
            except subprocess.CalledProcessError as error:
                assert error.returncode == 128, "SIO3 unreadable packed ref storage"
            else:
                raise AssertionError("SIO3 unknown packed storage became unborn")
