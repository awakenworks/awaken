"""Refuse an awaited physical effect whose in-flight future nobody owns.

Root-cause class (a 2000-commit review found it repeatedly): an `async fn`
obtains a claim, lease or authority, then `.await`s a call that creates a host
resource — a mount, a sandbox, a pod, a process, a repository clone — with the
in-flight future held only in a local. When the caller is dropped mid-await
the resource exists on the host and no record says whose it is, so no later
disposal, adoption or recovery path can find it.

Two shapes in the tree are correct and the rule is written from them:

  1. `awaken-sandbox-local/src/memory_mount_owner.rs`: the mount future is
     stored into `self.pending` BEFORE it is polled, and the next acquire fails
     closed while `pending.is_some()`. The owner outlives the caller.
  2. `awaken-sandbox-container/src/staged_mounts.rs`: `StagedMounts` implements
     `Drop`, and `retain_unknown_memory` is called immediately before the
     `mount()` call. A dropped caller leaves "record and maybe a mount", never
     "mount and no record" (`the_production_path_claims_before_it_mounts`).

So an awaited call to a registered resource-creating effect is admitted only
when, inside the same function body and textually BEFORE the await, either

  (a) the future or claim is registered into a field or variable named
      `pending` / `in_flight` (assignment, `let`, or `.insert(` / `.set(`), or
  (b) a responsibility record is written first — a call whose name starts with
      `retain_`, `claim_` or `record_` occurs earlier in the body, or
  (c) the enclosing `impl` type implements `Drop` anywhere in its crate, so the
      owner itself retains what the cancelled caller left behind.

Anything else is reported with file:line and the effect name.

Whole-repo scan plus an exemption registry, never a file allow-list: a list
freezes today's instances, a scan also refuses tomorrow's. The untriaged
remainder is ratcheted, so a *new* unowned effect is refused today rather than
only after the migration finishes.
"""

from __future__ import annotations

import re
from pathlib import Path

import _crate_boundary_workspace


# Effect methods whose successful completion creates a resource on the host.
# Key: the method name as it appears at the call site. Value: what the call
# creates, so a later reader can judge whether a sibling belongs here. Every
# name is a real production symbol; names that were proposed but do not exist
# (`spawn_hand`, `create_pod`) or that are synchronous and therefore never
# awaited (`provision_repo_at`, a plain `fn` running `git clone`) are not
# listed, because a registry entry that can never match guards nothing.
EFFECT_METHODS: dict[str, str] = {
    "mount": "a Memory participant mount on the host (pc::MemoryMounter::mount)",
    "materialize_memory_binds": "every Memory mount of a container staging",
    "create_sandbox_for_effect": "a local/namespace sandbox root and its mounts",
    "create_sandbox": "a local/namespace sandbox root and its mounts",
    "create_sandbox_inner": "a local/namespace sandbox root and its mounts",
    "realize_repository": "a repository clone inside the sandbox root",
    "realize_mount": "one realized mount inside a local sandbox root",
    "create_container_for_effect": "a container on the runtime (docker/podman/k8s)",
    "create_container": "a container on the runtime (docker/podman/k8s)",
    "create_environment_for_effect": "a sandbox environment on the provider",
    "create_environment": "a sandbox environment on the provider",
    "create_unattached_environment": "a sandbox environment no aggregate effect owns",
    "restore_environment": "a sandbox environment restored from a checkpoint",
    "realize_restore_container": "a container restored from a checkpoint",
    "create_or_verify": "a Kubernetes object (Pod, PVC, Secret) via Api::create",
    "create_or_verify_with_status": "a Kubernetes object via Api::create",
    "create_or_verify_exact": "a Kubernetes object via Api::create",
    "create_or_verify_with_status_exact": "a Kubernetes object via Api::create",
    # `spawn`/`spawn_agent` on the container runtime port are absent on
    # purpose: they take an `AdmittedExec`, which can only be built by taking
    # an attempt from an environment's ledger, so the compiler now states the
    # obligation this registry used to. A name cannot be registered for the
    # bare `spawn` anyway — it would match every `tokio::spawn` in the tree —
    # which is exactly how that edge stayed invisible here until it was typed.
    "spawn_agent_process": "an agent process inside a container",
    "spawn_prepared": "a supervised child process inside a local sandbox",
    "prewarm": "warm containers created ahead of demand",
    # The podman backend never reached this registry: it creates through
    # `self.run(&args)`, a name far too general to match. Its own port name is
    # the precise leaf, and the docker backend spells it identically.
    "prepare_for_effect": "a container prepared on the runtime for one effect",
}

# Key: "<relative path>::<fn name>". Value: why the await is cancel-safe
# despite matching none of the three shapes — name the owner that survives the
# caller, so a later reader can retire the entry when the code changes.
CANCEL_SAFETY_EXEMPTIONS: dict[str, str] = {
    "crates/worker/awaken-sandbox-container/src/pool.rs::prewarm_to": (
        "delegates to WarmContainerPool::prewarm, which claims the warm scope"
    ),
    # Matches on the name only. The callee is a control-plane domain command
    # (`CreateEnvironmentCommand` keyed by `command_id`), preceded by a durable
    # audit write and reconciled by `already_applied_output`; no mount, sandbox,
    # pod, process or clone is created, so there is no host resource to strand.
    "crates/control/awaken-admin-assistant/src/lib.rs::invoke": (
        "control-plane command idempotent under command_id, not a host effect"
    ),
}

# The shape predates the gate. Converting every instance at once would be a
# migration, not a fix, so the untriaged remainder is ratcheted: the count may
# fall and may not rise. Lower it as call sites are owned or registered.
#
# It rose once, from 67 to 71, and that was the gate telling the truth for the
# first time: the old admission rule matched any `claim_*`-shaped name, so two
# pure predicates were admitting five real Kubernetes creations — the Sandbox
# Pod among them — and the podman backend had no registered leaf at all. A
# number that falls because the rule stopped looking is not progress.
CANCEL_SAFETY_BASELINE = 54

FN_HEAD = re.compile(r"\bfn\s+([A-Za-z_]\w*)\s*")
IMPL_HEAD = re.compile(r"\bimpl\b")
AWAIT = re.compile(r"\s*\.\s*await\b")
# (a) the in-flight future or claim is registered under a name that says so.
PENDING_REGISTRATION = re.compile(
    r"(?:\blet\s+(?:mut\s+)?(?:pending|in_flight)\b"
    r"|\b(?:pending|in_flight)\s*=[^=]"
    r"|\b(?:pending|in_flight)\s*\.\s*(?:insert|set|replace|push|get_or_insert\w*)\s*\()"
)
# (b) a responsibility record is written before the physical call.
#
# This was a name shape (`retain_*`/`claim_*`/`record_*`) until a full read of
# the tree found it admitting `continuation::claim_name(` and
# `claim_required(` — two *pure functions* that compute a string and a boolean.
# Five real Kubernetes creations, the Sandbox Pod itself among them, were
# admitted because a predicate happened to be spelled like a claim. A shape can
# only guess; a recorder has to be named, together with the reader that reads
# its record back, because a record nobody reads is not a record.
#
# Key: the recording call, exactly as it is written at the call site.
# Value: the reader that recovers the record, so an entry can be retired when
# its reader disappears.
RESPONSIBILITY_RECORDERS: dict[str, str] = {
    "retain_unknown_memory": (
        "awaken_sandbox_container::staged_mounts::StagedMounts::into_parts refuses while a "
        "claim is unsettled, and the existing restoration consumer drains the staging root"
    ),
    "retain_for_restoration": (
        "awaken_sandbox_container::provider_realization::acquire_restore_sandbox observes the "
        "retained restore target before creating another"
    ),
    "record_hand_lifecycle": (
        "awaken_session_executor::session_environment::session_hand observes the recorded Hand "
        "lifecycle when it rebuilds or revokes the Session Hand"
    ),
}
RESPONSIBILITY_RECORD = re.compile(
    r"\b(?:" + "|".join(re.escape(name) for name in RESPONSIBILITY_RECORDERS) + r")\s*\("
)
DROP_IMPL = re.compile(r"\bimpl(?:<[^>]*>)?\s+Drop\s+for\s+([A-Za-z_]\w*)")
# (d) the effect is wrapped in a `ClaimedEffect`, whose claim is taken by value
# before the future exists and whose `Drop` runs the retention on an unsettled
# drop. This is the shape the other three are migrating towards: it is the only
# one where the guarantee is the type's rather than the call site's, and the
# Kani harness `claim_retention_runs_once_only_for_an_unsettled_drop` proves it
# once for every site instead of per site.
MINTED_CLAIM = re.compile(r"\bEffectClaim\s*::\s*(?:self_recording|durable|in_process)\s*\(")


def _bracket_end(source: str, open_index: int, opener: str, closer: str) -> int:
    depth = 0
    index = open_index
    while index < len(source):
        if source[index] == opener:
            depth += 1
        elif source[index] == closer:
            depth -= 1
            if depth == 0:
                return index
        index += 1
    return len(source) - 1


def _skip_generics(source: str, index: int) -> int:
    if index < len(source) and source[index] == "<":
        return _bracket_end(source, index, "<", ">") + 1
    return index


def function_bodies(production: str) -> list[dict]:
    """Every `fn name(...) { body }` with its body span; trait declarations
    ending in `;` have no body and are skipped."""
    bodies: list[dict] = []
    for head in FN_HEAD.finditer(production):
        cursor = _skip_generics(production, head.end())
        if cursor >= len(production) or production[cursor] != "(":
            continue
        params_end = _bracket_end(production, cursor, "(", ")")
        brace = production.find("{", params_end)
        semicolon = production.find(";", params_end)
        if brace < 0 or 0 <= semicolon < brace:
            continue
        end = _bracket_end(production, brace, "{", "}")
        bodies.append({"name": head.group(1), "start": head.start(), "body": brace, "end": end})
    return bodies


def impl_blocks(production: str) -> list[dict]:
    """Every `impl … Type {` with the implemented type's bare name and span."""
    blocks: list[dict] = []
    for head in IMPL_HEAD.finditer(production):
        brace = production.find("{", head.end())
        if brace < 0:
            continue
        header = production[head.end() : brace]
        if ";" in header:
            continue
        subject = header.split(" for ", 1)[1] if " for " in header else header
        subject = re.sub(r"<.*", "", subject.strip().lstrip("<"))
        subject = subject.split("::")[-1].strip()
        if not re.fullmatch(r"[A-Za-z_]\w*", subject or ""):
            continue
        blocks.append({"type": subject, "start": head.start(), "end": _bracket_end(production, brace, "{", "}")})
    return blocks


def _enclosing(items: list[dict], offset: int, start_key: str = "start") -> dict | None:
    inner = None
    for item in items:
        if item[start_key] <= offset <= item["end"] and (inner is None or item[start_key] > inner[start_key]):
            inner = item
    return inner


def _call_sites(production: str) -> list[tuple[str, int, int]]:
    """(effect, call start, await end) for every awaited registered effect."""
    sites: list[tuple[str, int, int]] = []
    names = "|".join(sorted(EFFECT_METHODS, key=len, reverse=True))
    pattern = re.compile(rf"(?<![\w])(?:\.\s*)?({names})\s*(?:::<[^>]*>\s*)?\(")
    for match in pattern.finditer(production):
        preceding = production[max(0, match.start() - 8) : match.start()]
        if re.search(r"\bfn\s*$", preceding):
            continue  # the declaration, not a call
        close = _bracket_end(production, match.end() - 1, "(", ")")
        awaited = AWAIT.match(production, close + 1)
        if awaited is None:
            continue  # a stored or synchronous call: no cancellation point here
        sites.append((match.group(1), match.start(), awaited.end()))
    return sites


def _crate_root(path: Path) -> Path:
    parts = path.parts
    if "src" in parts:
        return Path(*parts[: len(parts) - 1 - parts[::-1].index("src")])
    return path.parent


def _drop_owners(sources: dict[Path, str]) -> dict[Path, set[str]]:
    owners: dict[Path, set[str]] = {}
    for path, production in sources.items():
        owners.setdefault(_crate_root(path), set()).update(DROP_IMPL.findall(production))
    return owners


def unowned_effects(sources: dict[Path, str], repo_root: Path) -> list[tuple[str, str]]:
    """(key, message) for each awaited effect no owner survives the caller of."""
    drop_owners = _drop_owners(sources)
    found: list[tuple[str, str]] = []
    for path, production in sources.items():
        sites = _call_sites(production)
        if not sites:
            continue
        bodies = function_bodies(production)
        impls = impl_blocks(production)
        crate_drops = drop_owners.get(_crate_root(path), set())
        for effect, call_start, _ in sites:
            function = _enclosing(bodies, call_start, "body")
            if function is None:
                continue  # a const initialiser or macro; nothing awaits there
            before = production[function["body"] : call_start]
            if PENDING_REGISTRATION.search(before):
                continue  # (a) the future is owned by a `pending`/`in_flight` slot
            if RESPONSIBILITY_RECORD.search(before):
                continue  # (b) a registered recorder wrote its record first
            if MINTED_CLAIM.search(before):
                continue  # (d) the effect carries a claim minted before its future
            owner = _enclosing(impls, function["start"])
            if owner is not None and owner["type"] in crate_drops:
                continue  # (c) the owning type retains on Drop
            relative = path.relative_to(repo_root).as_posix()
            line = production[:call_start].count("\n") + 1
            key = f"{relative}::{function['name']}"
            found.append(
                (
                    key,
                    f"{relative}:{line}: `{function['name']}` awaits `{effect}()` — "
                    f"{EFFECT_METHODS[effect]} — with the in-flight future held only in a "
                    "local. A caller dropped mid-await leaves the resource on the host with "
                    "no responsibility record. Store the future into a `pending`/`in_flight` "
                    "slot before polling it, hand the effect to a `ClaimedEffect` whose "
                    "`EffectClaim` is minted first, call one of RESPONSIBILITY_RECORDERS before the "
                    "effect, give the owning type a `Drop` that retains, or record the "
                    "surviving owner in CANCEL_SAFETY_EXEMPTIONS",
                )
            )
    return found


def _production_sources(crates: Path) -> dict[Path, str]:
    """Production-reachable modules, each projected with cfg(test) == false.

    A file behind `#[cfg(test)] mod tests;` in its parent is test-only even
    though nothing inside it says so, so module reachability from the Cargo
    roots decides which files are scanned, and the per-file projection then
    blanks inline test items.
    """
    reachable = _crate_boundary_workspace.production_module_files(
        sorted(crates.glob("*/*/Cargo.toml"))
    )
    sources: dict[Path, str] = {}
    for path in sorted(crates.glob("*/*/src/**/*.rs")):
        if path.resolve() not in reachable:
            continue
        raw = path.read_text(encoding="utf-8", errors="ignore")
        # A file naming no registered effect and no `Drop for` can neither
        # report nor admit anything, so the (costly) projection is skipped.
        if "Drop for" not in raw and not any(name in raw for name in EFFECT_METHODS):
            continue
        sources[path] = _crate_boundary_workspace.production_rust(raw)
    return sources


def _ratcheted(
    found: list[tuple[str, str]],
    exemptions: dict[str, str],
    baseline: int,
    label: str,
    registry: str,
) -> list[str]:
    live = [message for key, message in found if key not in exemptions]
    seen = {key for key, _ in found}
    errors = [
        f"{registry} names {key}, which no longer holds; drop the stale exemption"
        for key in sorted(exemptions)
        if key not in seen
    ]
    if len(live) > baseline:
        errors.extend(sorted(live))
        errors.append(
            f"{len(live)} {label}, baseline {baseline}: {len(live) - baseline} new one(s). "
            f"Own the in-flight effect, or record the decision in {registry}."
        )
    elif len(live) < baseline:
        errors.append(
            f"{len(live)} {label}, below the baseline of {baseline}. Lower the baseline to "
            f"{len(live)} so the ground gained is held."
        )
    return errors


# An `AdmittedExec` is the proof that a container process was written into an
# environment's ledger before it started. `unrecorded` builds one without a
# ledger, and exactly one production caller may: the reaper's own exec IS the
# sweep, not something the sweep must later find. Every other one would be a
# spawn edge with no record — the defect the type exists to make impossible —
# so the bypass is counted rather than trusted. This is what replaces the
# `spawn`/`spawn_agent` rows that used to sit in EFFECT_METHODS: the registry
# could never name the bare `spawn` without matching every `tokio::spawn`, and
# a name it cannot state is a name it cannot guard.
UNRECORDED_ADMISSION = re.compile(r"AdmittedExec::unrecorded\s*\(")
UNRECORDED_ADMISSION_BASELINE = 1


def unrecorded_admissions(crates: Path, repo_root: Path) -> list[tuple[str, str]]:
    """Every production site that starts a process with no ledger behind it."""
    found: list[tuple[str, str]] = []
    for path, text in _production_sources(crates).items():
        for number, line in enumerate(text.splitlines(), start=1):
            if line.lstrip().startswith(("//", "#")):
                continue
            if UNRECORDED_ADMISSION.search(line):
                key = f"{path.relative_to(repo_root).as_posix()}::{number}"
                found.append((
                    key,
                    f"{key}: admits an exec no environment ledger records. Only the "
                    "reaper's own sweep may; take the attempt from ExecAttempts instead",
                ))
    return found


def live_findings(repo_root: Path, crates: Path) -> list[tuple[str, str]]:
    """Every unexempted finding on the tree, for triage and for the baseline."""
    found = unowned_effects(_production_sources(crates), repo_root)
    return [(key, message) for key, message in found if key not in CANCEL_SAFETY_EXEMPTIONS]


def check_all(repo_root: Path, crates: Path) -> list[str]:
    return _ratcheted(
        unowned_effects(_production_sources(crates), repo_root),
        CANCEL_SAFETY_EXEMPTIONS,
        CANCEL_SAFETY_BASELINE,
        "awaited physical effects with no owner surviving the caller",
        "CANCEL_SAFETY_EXEMPTIONS",
    ) + _ratcheted(
        unrecorded_admissions(crates, repo_root),
        {},
        UNRECORDED_ADMISSION_BASELINE,
        "production execs admitted with no ledger behind them",
        "the reaper's own sweep",
    )


def selftest() -> None:
    """Cause/effect over one synthetic crate, so the table is the specification.

    Causes: C1 a registered effect is awaited inside a function body; C2 a
    `pending`/`in_flight` registration precedes the await in that body; C3 a
    call to a *registered* recorder (RESPONSIBILITY_RECORDERS) precedes it;
    C4 the enclosing impl type
    has an `impl Drop` in the crate; C5 the code is test-only (`cfg(test)`);
    C6 the registration exists but only AFTER the await. Effects: E1 reported,
    E2 admitted.

    | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
    | T1   | T  | F  | F  | F  | F  | F  | E1     |
    | T2   | T  | T  | F  | F  | F  | F  | E2     |
    | T3   | T  | F  | T  | F  | F  | F  | E2     |
    | T4   | T  | F  | F  | T  | F  | F  | E2     |
    | T5   | T  | F  | F  | F  | T  | F  | E2     |
    | T6   | T  | F  | F  | F  | F  | T  | E1     |
    | T7   | T  | F  | *  | F  | F  | F  | E1     |
    | T8   | T  | F  | F  | F  | F  | F  | E2 when an EffectClaim is minted first |

    T7 is C3 spelled like a recorder but not registered: a name shape cannot
    tell a record from a predicate. `continuation::claim_name` and
    `claim_required` are pure functions, and while the rule matched shapes they
    admitted five real Kubernetes creations, the Sandbox Pod among them.

    The synthetic crate is written to a tempdir and scanned through the same
    `check_all` the gate runs, so the file glob, cfg(test) projection and
    per-crate Drop lookup are exercised, not just the matcher.
    """
    import tempfile

    unregistered = (
        "pub struct Stage { slot: Option<u8> }\n"
        "impl Stage {\n"
        "    pub async fn build(&mut self, m: &dyn Mounter) -> Result<(), E> {\n"
        "        let handle = m.mount(&reference, &path, access).await?;\n"
        "        self.slot = Some(handle);\n"
        "        Ok(())\n"
        "    }\n"
        "}\n"
    )
    pending_first = unregistered.replace(
        "        let handle = m.mount(&reference, &path, access).await?;\n",
        "        self.pending = Some(m.mount(&reference, &path, access));\n"
        "        let handle = self.pending.as_mut().unwrap().await?;\n",
    )
    retain_first = unregistered.replace(
        "        let handle = m.mount(",
        "        self.retain_unknown_memory(reference.clone(), path.clone());\n"
        "        let handle = m.mount(",
    )
    # Shaped like a recorder, registered nowhere, and — as in the real tree —
    # a pure function that computes a name.
    claim_shaped = unregistered.replace(
        "        let handle = m.mount(",
        "        let name = claim_name(&plan, continuity);\n"
        "        let required = claim_required(configured, continuity);\n"
        "        let handle = m.mount(",
    )
    drop_owner = unregistered + "impl Drop for Stage {\n    fn drop(&mut self) {}\n}\n"
    test_only = "#[cfg(test)]\nmod tests {\n" + unregistered + "}\n"
    registered_after = unregistered.replace(
        "        self.slot = Some(handle);\n",
        "        self.pending = Some(handle);\n",
    )

    def write_crate(crates: Path, relative: str, text: str) -> None:
        source = crates / relative
        source.parent.mkdir(parents=True, exist_ok=True)
        source.write_text(text, encoding="utf-8")
        manifest = source.parent.parent / "Cargo.toml"
        if not manifest.exists():
            manifest.write_text(
                f'[package]\nname = "{source.parent.parent.name}"\nversion = "0.0.0"\n',
                encoding="utf-8",
            )

    def scan(text: str, extra: dict[str, str] | None = None) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            crates = Path(directory) / "crates"
            write_crate(crates, "worker/example/src/lib.rs", text)
            for relative, body in (extra or {}).items():
                write_crate(crates, relative, body)
            found = unowned_effects(_production_sources(crates), Path(directory))
            return [key for key, _ in found]

    assert scan(unregistered) == ["crates/worker/example/src/lib.rs::build"], "T1 reported"
    assert scan(pending_first) == [], "T2 pending registered before the await is admitted"
    assert scan(retain_first) == [], "T3 a registered recorder before the await is admitted"
    claimed = unregistered.replace(
        "        let handle = m.mount(",
        "        let claim = EffectClaim::in_process(move || record.retain());\n"
        "        let handle = m.mount(",
    )
    assert scan(claimed) == [], "T8 an EffectClaim minted before the effect is admitted"
    assert scan(claim_shaped) == ["crates/worker/example/src/lib.rs::build"], (
        "T7 a claim_-shaped pure function is not a responsibility record"
    )
    assert scan(drop_owner) == [], "T4 an owning type with Drop is admitted"
    assert scan(
        unregistered + "mod guard;\n",
        {"worker/example/src/guard.rs": "impl Drop for Stage { fn drop(&mut self) {} }\n"},
    ) == [], "T4 Drop declared elsewhere in the same crate is admitted"
    assert scan(unregistered, {"worker/other/src/lib.rs": "impl Drop for Stage { fn drop(&mut self) {} }\n"}) == [
        "crates/worker/example/src/lib.rs::build"
    ], "T4 Drop in a different crate does not own this effect"
    assert scan(test_only) == [], "T5 test-only code is ignored"
    assert scan(
        "#[cfg(test)]\nmod tests;\n", {"worker/example/src/tests.rs": unregistered}
    ) == [], "T5 a file reached only through `#[cfg(test)] mod tests;` is ignored"
    assert scan("mod stage;\n", {"worker/example/src/stage.rs": unregistered}) == [
        "crates/worker/example/src/stage.rs::build"
    ], "T1 a production module file is scanned"
    assert scan(registered_after) == ["crates/worker/example/src/lib.rs::build"], "T6 registration after the await is reported"

    # An effect that is not awaited is not a cancellation point (a stored
    # future is exactly shape 1), and a free function carries its own name.
    stored = unregistered.replace(
        "        let handle = m.mount(&reference, &path, access).await?;\n",
        "        let future = m.mount(&reference, &path, access);\n        let handle = future;\n",
    )
    assert scan(stored) == [], "an unawaited call is not a cancellation point"
    free = (
        "pub async fn stage(m: &dyn Mounter) -> Result<(), E> {\n"
        "    materialize_memory_binds(&spec, &mut plan, &mut staged).await?;\n    Ok(())\n}\n"
    )
    assert scan(free) == ["crates/worker/example/src/lib.rs::stage"], "a free-function effect names its caller"

    # The ratchet itself: above the baseline fails, at it admits, below it
    # demands a lower number, an exemption removes one, a stale one rejects.
    found = [("A", "a"), ("B", "b")]
    assert _ratcheted(found, {}, 1, "x", "REG"), "ratchet: above baseline rejects"
    assert not _ratcheted(found, {}, 2, "x", "REG"), "ratchet: at baseline admits"
    assert _ratcheted(found, {}, 3, "x", "REG"), "ratchet: below baseline demands a lower number"
    assert not _ratcheted(found, {"A": "why"}, 1, "x", "REG"), "ratchet: an exemption removes one"
    assert _ratcheted(found, {"C": "why"}, 2, "x", "REG"), "ratchet: a stale exemption rejects"

    with tempfile.TemporaryDirectory() as directory:
        crates = Path(directory) / "crates"
        write_crate(crates, "worker/example/src/lib.rs", unregistered)
        assert check_all(Path(directory), crates), "an unregistered instance fails check_all"

    print("_cancel_safety_fitness selftest: the decision table holds")
