#!/usr/bin/env python3
"""Fail when a harness script can leak an anonymous Docker volume.

Images such as postgres, minio, registry, k3s, buildkit, jenkins and vault
declare VOLUME. `docker run` without a mount for that path creates an anonymous
volume, and `docker rm` keeps it unless asked to remove volumes; nothing can
attribute the leftover to a run, so it stays until someone prunes by hand.

Decision table (command -> verdict):

| command                                              | verdict                      |
| `docker rm` without -v / --volumes                   | violation                    |
| `docker run|create` of a listed image, no mount for  | violation                    |
|   every path the image declares (--tmpfs, -v,        |                              |
|   --volume, --mount)                                 |                              |
| same, foreground (no -d) with --rm                   | allowed: --rm removes volumes|
| any line carrying `docker-volume-lint: allow <why>`  | allowed (also on prior line) |

Usage: check_docker_volumes.py [path ...]   (default: e2e and scripts)
"""

from __future__ import annotations

import pathlib
import re
import shlex
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
DEFAULT_DIRS = ("e2e", "scripts")
ALLOW = "docker-volume-lint: allow"
INPUT_GLOBS: tuple[str, ...] = ("e2e/**/*.sh", "scripts/**/*.sh", "scripts/ci/check_docker_volumes.py")

# image regex -> data paths the image declares as VOLUME (a mount at or above a
# path covers it). Versions differ (postgres 18 moved the volume up a level).
IMAGES = (
    (r"postgres", ("/var/lib/postgresql",)),
    (r"minio/minio", ("/data",)),
    (r"registry", ("/var/lib/registry",)),
    (r"rancher/k3s", ("/var/lib/rancher/k3s", "/var/lib/kubelet", "/var/lib/cni", "/var/log")),
    (r"moby/buildkit", ("/var/lib/buildkit",)),
    (r"(?:[\w.-]+/)?vault", ("/vault/file", "/vault/logs")),
    (r"[\w.-]*jenkins[\w.-]*", ("/var/jenkins_home",)),
)
# Options whose next token is a value, never the image.
VALUE_OPTIONS = {
    "--name", "-e", "--env", "--env-file", "--network", "--net", "-l", "--label",
    "--hostname", "-h", "-p", "--publish", "-u", "--user", "--entrypoint",
    "-w", "--workdir", "--cap-add", "--health-cmd", "--health-interval",
    "--health-timeout", "--health-retries", "--restart", "--platform", "--pull",
    "--add-host", "--ulimit", "--memory", "-m", "--cpus", "--shm-size",
    "--security-opt", "--log-driver", "--log-opt", "--user", "--volumes-from",
}
TERMINATORS = (";", "&&", "||", "|", ")", ">", "<", "&")


def logical_lines(text: str):
    """Yield (first_line_number, line) with backslash continuations joined."""
    buffer, start = "", 0
    for number, raw in enumerate(text.splitlines(), 1):
        if not buffer:
            start = number
        if raw.rstrip().endswith("\\"):
            buffer += raw.rstrip()[:-1] + " "
            continue
        yield start, buffer + raw
        buffer = ""
    if buffer:
        yield start, buffer


def command_text(line: str, begin: int) -> str:
    """Text from `begin` to the first terminator outside quotes."""
    quote, out, index = "", [], begin
    while index < len(line):
        char = line[index]
        if quote:
            if char == quote:
                quote = ""
        elif char in "'\"":
            quote = char
        elif char == "#" and (index == 0 or line[index - 1].isspace()):
            break
        elif any(line.startswith(t, index) for t in TERMINATORS):
            break
        out.append(char)
        index += 1
    return "".join(out)


def assignments(text: str) -> dict[str, str]:
    found = {}
    for match in re.finditer(r"^\s*(?:readonly\s+|local\s+|export\s+)?([A-Za-z_]\w*)=[\"']?([^\"'\s]+)", text, re.M):
        found.setdefault(match.group(1), match.group(2))
    return found


def substitute(token: str, variables: dict[str, str]) -> str:
    def replace(match: re.Match) -> str:
        name = match.group(1) or match.group(3)
        default = match.group(2)
        return variables.get(name, default if default is not None else match.group(0))

    return re.sub(r"\$(?:\{(\w+)(?::?[-=]([^}]*))?\}|(\w+))", replace, token)


def mount_targets(tokens: list[str]) -> list[str]:
    targets = []
    for index, token in enumerate(tokens):
        value = None
        if token in ("--tmpfs", "-v", "--volume", "--mount") and index + 1 < len(tokens):
            value = tokens[index + 1]
            flag = token
        elif token.startswith(("--tmpfs=", "--volume=", "--mount=")):
            flag, _, value = token.partition("=")
        else:
            continue
        if flag == "--mount":
            for part in value.split(","):
                key, _, item = part.partition("=")
                if key in ("target", "dst", "destination"):
                    targets.append(item)
        elif flag == "--tmpfs":
            targets.append(value.split(":")[0])
        else:
            pieces = value.split(":")
            targets.append(pieces[1] if len(pieces) > 1 else pieces[0])
    return targets


def covered(path: str, targets: list[str]) -> bool:
    return any(path == t.rstrip("/") or path.startswith(t.rstrip("/") + "/") or t.rstrip("/").startswith(path + "/")
               for t in targets if t)


def find_image(tokens: list[str]):
    skip = False
    for token in tokens:
        if skip:
            skip = False
            continue
        if token in VALUE_OPTIONS:
            skip = True
            continue
        if token.startswith("-"):
            continue
        base = token.split("@")[0]
        head, _, last = base.rpartition("/")
        base = (head + "/" if head else "") + last.split(":")[0]
        for pattern, paths in IMAGES:
            if re.fullmatch(r"(?:.*/)?" + pattern, base):
                return base, paths
    return None


def rm_removes_volumes(flags: list[str]) -> bool:
    for flag in flags:
        if flag == "--volumes" or (flag.startswith("-") and not flag.startswith("--") and "v" in flag[1:]):
            return True
    return False


def check_text(text: str, name: str = "<text>") -> list[str]:
    problems = []
    variables = assignments(text)
    lines = text.splitlines()
    for number, line in logical_lines(text):
        if line.lstrip().startswith("#"):
            continue
        for match in re.finditer(r"\bdocker\s+(?:container\s+)?(run|create|rm)\b", line):
            before = lines[number - 2] if number >= 2 else ""
            if ALLOW in line or ALLOW in before:
                continue
            try:
                tokens = shlex.split(command_text(line, match.end()), posix=True)
            except ValueError:
                tokens = command_text(line, match.end()).split()
            tokens = [substitute(t, variables) for t in tokens]
            verb = match.group(1)
            if verb == "rm":
                flags = []
                for token in tokens:
                    if not token.startswith("-"):
                        break
                    flags.append(token)
                if not rm_removes_volumes(flags):
                    problems.append(f"{name}:{number}: `docker rm` without -v/--volumes keeps the container's volumes")
                continue
            found = find_image(tokens)
            if not found:
                continue
            image, paths = found
            detached = any(t in ("-d", "--detach") or (t.startswith("-") and not t.startswith("--") and "d" in t[1:]) for t in tokens
                           if t.startswith("-") and t not in VALUE_OPTIONS)
            if "--rm" in tokens and not detached and verb == "run":
                continue
            targets = mount_targets(tokens)
            missing = [p for p in paths if not covered(p, targets)]
            if missing:
                problems.append(f"{name}:{number}: `docker {verb}` of {image} declares VOLUME {', '.join(missing)}; "
                                "add --tmpfs or a named volume (or --rm for a foreground run)")
    return problems


def scripts(paths: list[pathlib.Path]):
    for path in paths:
        if path.is_file():
            yield path
        elif path.is_dir():
            yield from sorted(p for p in path.rglob("*.sh") if "node_modules" not in p.parts)


def main(argv: list[str]) -> int:
    if any(a.startswith("-") for a in argv):
        print("usage: scripts/ci/check_docker_volumes.py [path ...]", file=sys.stderr)
        return 2
    roots = [pathlib.Path(a) for a in argv] or [ROOT / d for d in DEFAULT_DIRS if (ROOT / d).exists()]
    problems = []
    for script in scripts(roots):
        try:
            problems += check_text(script.read_text(encoding="utf-8"), str(script.relative_to(ROOT)) if script.is_relative_to(ROOT) else str(script))
        except UnicodeDecodeError:
            continue
    if problems:
        print("check_docker_volumes: anonymous Docker volume leaks:", file=sys.stderr)
        print("\n".join(problems), file=sys.stderr)
        return 1
    print("check_docker_volumes: every stateful container has an explicit mount and every docker rm removes volumes")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
