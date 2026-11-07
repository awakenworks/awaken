"""Qualify a candidate on its own ref, then fast-forward main at its configured writer."""

from __future__ import annotations

import argparse
import re
import shlex
import subprocess
import sys
import time
from urllib.parse import urlsplit


MAIN = "refs/heads/main"
SSH_HOST = re.compile(r"(?:[A-Za-z0-9_.-]+@)?[A-Za-z0-9][A-Za-z0-9_.-]*")


def ssh_repository(url: str) -> tuple[list[str], str]:
    if url.startswith("ssh://"):
        parsed = urlsplit(url)
        host = f"{parsed.username}@" if parsed.username else ""
        host += parsed.hostname or ""
        port = ["-p", str(parsed.port)] if parsed.port else []
        repository = parsed.path
        if parsed.password or parsed.query or parsed.fragment:
            raise ValueError("the configured SSH push URL must name only a repository")
    else:
        if re.match(r"^[A-Za-z]:[/\\]", url):
            raise ValueError("a local drive is not an SSH push URL")
        host, separator, repository = url.partition(":")
        port = []
        if not separator:
            raise ValueError("expected an SSH push URL")
    if not SSH_HOST.fullmatch(host) or not repository:
        raise ValueError("expected an SSH push URL with a named host and repository")
    return ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", *port, host], repository


REMOTE = r'''
set -eu
repo=$1
base=$2
head=$3
tree=$4
mode=$5
candidate=$6
case "$repo" in
    "~/"*) repo="$HOME/${repo#??}" ;;
esac
cd -- "$repo"
common=$(git rev-parse --path-format=absolute --git-common-dir)
writer=$(git config --get awaken.mainWriter) || {
    echo "The receiving repository does not configure awaken.mainWriter" >&2; exit 1;
}
cd -- "$writer"
test "$(git rev-parse --path-format=absolute --git-common-dir)" = "$common"
test -z "$(git rev-parse --show-prefix)"
test -x "$(git rev-parse --git-path hooks/reference-transaction)"
test -z "$(git --no-optional-locks status --porcelain=v1 --untracked-files=all)" || {
    echo "The integration worktree has local changes" >&2; exit 1;
}
test "$(git rev-parse refs/heads/main)" = "$base" || {
    echo "Remote main changed; fetch and rebase the candidate" >&2; exit 1;
}
checked_out=$(git for-each-ref --format='%(worktreepath)' refs/heads/main)
test -z "$checked_out" || {
    echo "Remote main is checked out at $checked_out" >&2; exit 1;
}
if test "$mode" = integrate; then
    test "$(git rev-parse "$candidate")" = "$head"
    test "$(git rev-parse "$head^{tree}")" = "$tree"
    git merge-base --is-ancestor "$base" "$head"
    test -z "$(git rev-list --merges "$base..$head")"
    unset AWAKEN_ALLOW_MAIN_REWRITE
    git update-ref -m "integrate qualified candidate $head" refs/heads/main "$head" "$base"
    git rev-parse refs/heads/main
else
    printf '%s\n' "Ready to integrate at $writer"
fi
'''


def git_output(*arguments: str) -> str:
    return subprocess.check_output(["git", *arguments], text=True).strip()


def publish(remote: str, *, check_only: bool) -> None:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", remote):
        raise ValueError("select a configured Git remote by name")
    if git_output("status", "--porcelain=v1"):
        raise ValueError("the candidate worktree must be clean")
    url = git_output("remote", "get-url", "--push", remote)
    ssh, repository = ssh_repository(url)
    base = git_output("rev-parse", f"refs/remotes/{remote}/main")
    head = git_output("rev-parse", "HEAD")
    tree = git_output("rev-parse", "HEAD^{tree}")
    if head == base:
        raise ValueError("the candidate already matches the fetched main")
    subprocess.run(["git", "merge-base", "--is-ancestor", base, head], check=True)
    if git_output("rev-list", "--merges", f"{base}..{head}"):
        raise ValueError("the candidate adds merge commits")
    candidate = f"refs/heads/codex/integration/{head}"

    def remote_step(mode: str) -> None:
        command = shlex.join(["sh", "-s", "--", repository, base, head, tree, mode, candidate])
        subprocess.run([*ssh, command], input=REMOTE.encode("utf-8"), check=True)

    remote_step("check")
    if check_only:
        return
    # The ordinary pre-push hook qualifies this exact checked-out commit once.
    from ci_performance import record_stage

    started = time.monotonic()
    uploaded = subprocess.run(["git", "push", remote, f"{head}:{candidate}"], check=False)
    elapsed = time.monotonic() - started
    record_stage("main-publication:qualification-and-transfer", elapsed, uploaded.returncode)
    uploaded.check_returncode()
    print(f"Candidate qualification and transfer: {elapsed:.1f}s", flush=True)
    if git_output("rev-parse", "HEAD") != head or git_output("status", "--porcelain=v1"):
        raise ValueError("the local source changed during qualification")
    started = time.monotonic()
    remote_step("integrate")
    expected = f"{head}\t{MAIN}"
    if git_output("ls-remote", url, MAIN) != expected:
        raise ValueError("remote main does not report the candidate after integration")
    elapsed = time.monotonic() - started
    record_stage("main-publication:integration-and-verification", elapsed, 0)
    print(f"Integration and verification: {elapsed:.1f}s", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("remote", nargs="?", default="origin")
    parser.add_argument("--preflight", action="store_true", help="check the writer before running any gate")
    args = parser.parse_args()
    try:
        publish(args.remote, check_only=args.preflight)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"main integration failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
