"""The change a failing land.py run judged, replayed when land.py re-checks main.

land.py bisects a failed group down to lane 1, then asks whether main itself
fails that group (V5). Narrowed to main against itself, an affected gate
reaches nothing and passes vacuously, so that run used to go whole -- for the
formal group the most expensive run of the pipeline (7802 s on 2026-11-18).
Instead land.py writes the paths lane 1 changed (``git diff -z --name-only``)
to a file named by ``AWAKEN_AFFECTED_REPLAY_PATHS``. The affected planners
read those paths instead of the diff since their base, so main runs exactly
the selection the failing run ran, measured against main's own sources.

A service group (postgres, reliability) narrowed to nothing starts no
substrate and would pass whatever the host's state; its planner turns a
replay that selects no suite into the whole group. The formal group always
runs its unnarrowed steps, so its replay stays the failing run's selection.
Outside land.py the variable is unset and nothing here changes a plan.
"""
from __future__ import annotations

import os
from pathlib import Path

VARIABLE = "AWAKEN_AFFECTED_REPLAY_PATHS"


def replayed_paths() -> list[str] | None:
    """The replayed change, or None when this run is not a re-check of main.

    An unreadable replay file is an error, never read as "nothing changed".
    """
    location = os.environ.get(VARIABLE, "")
    if not location:
        return None
    return sorted({os.fsdecode(name) for name in Path(location).read_bytes().split(b"\0") if name})
