"""Exercise main publication against the real reference transaction hook."""

from pathlib import Path
import subprocess
import tempfile
import unittest

from _native_shell import bash_executable
from formal_sources import detached_git_environment
from publish_main import MAIN, REMOTE


def verify_integration() -> None:
    with tempfile.TemporaryDirectory(prefix="awaken-integration-") as raw:
        root = Path(raw)
        repo, writer = root / "repo", root / "integration"
        environment = detached_git_environment()

        def git_at(directory: Path, *args: str) -> str:
            return subprocess.check_output(
                ["git", "-C", str(directory), "-c", "user.name=fixture",
                 "-c", "user.email=fixture@invalid", *args], text=True, env=environment,
            ).strip()

        repo.mkdir()
        git_at(repo, "init", "-q", "-b", "primary")
        git_at(repo, "commit", "--allow-empty", "-qm", "base")
        base = git_at(repo, "rev-parse", "HEAD")
        git_at(repo, "branch", "main", base)
        git_at(repo, "worktree", "add", "-qb", "integration", str(writer), base)
        git_at(repo, "config", "awaken.mainWriter", writer.as_posix())
        hook = repo / ".git" / "hooks" / "reference-transaction"
        hook.write_bytes(Path(__file__).with_name("check_main_linear.py").read_bytes().replace(b"\r\n", b"\n"))
        hook.chmod(0o755)
        git_at(repo, "commit", "--allow-empty", "-qm", "candidate")
        head = git_at(repo, "rev-parse", "HEAD")
        tree = git_at(repo, "rev-parse", "HEAD^{tree}")
        candidate = f"refs/heads/codex/integration/{head}"

        def remote_test(mode, *, old=base, expected_tree=tree):
            return subprocess.run(
                [bash_executable(), "-s", "--", repo.as_posix(), old, head, expected_tree, mode, candidate],
                input=REMOTE.encode("utf-8"), capture_output=True, env=environment,
            )

        assert remote_test("check").returncode == 0
        assert remote_test("integrate").returncode != 0, "candidate must have been received"
        git_at(repo, "update-ref", candidate, head)
        assert remote_test("integrate", expected_tree=head).returncode != 0
        dirty = writer / "user-work"
        dirty.write_text("preserve", encoding="utf-8")
        assert remote_test("check").returncode != 0
        assert dirty.read_text(encoding="utf-8") == "preserve"
        dirty.unlink()
        assert git_at(repo, "rev-parse", MAIN) == base
        result = remote_test("integrate")
        assert result.returncode == 0, result.stderr
        assert git_at(repo, "rev-parse", MAIN) == head
        assert git_at(writer, "rev-parse", "HEAD") == base
        assert remote_test("check").returncode != 0, "a stale base cannot start qualification"


class PublishMainTests(unittest.TestCase):
    def test_only_qualified_candidate_fast_forwards_a_clean_writer(self):
        verify_integration()


if __name__ == "__main__":
    unittest.main()
