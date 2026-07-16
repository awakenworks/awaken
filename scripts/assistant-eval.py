#!/usr/bin/env python3
"""Skill-opt eval harness for the Admin Assistant's agent-authoring skill.

Drives the live assistant (against the running backend on :38080) over a golden set of
natural-language authoring requests, K times each, and scores the PERSISTED draft config
against a per-case rubric. Reports task-success, compile rate, and — the metric that
matters for a structured-authoring skill — CONSISTENCY across repetitions.

Usage:
  python3 assistant-eval.py [K]             # measurement only (default)
  python3 assistant-eval.py [K] [CASE,...]  # run selected case ids
  python3 assistant-eval.py [K] --gate      # fail when a quality floor is missed
  python3 assistant-eval.py --self-test     # deterministic gate-logic check

The live-model result is not a proof: it is a repeated golden-set measurement.
Gate mode turns that measurement into an explicit CI/release signal while the
default remains non-blocking for exploratory runs.
"""
import argparse
import json
import sys
import time
import urllib.error
import urllib.request


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repetitions", nargs="?", type=int, default=3)
    parser.add_argument("cases", nargs="?", help="comma-separated case ids to run")
    parser.add_argument("--base-url", default="http://127.0.0.1:38080")
    parser.add_argument("--gate", action="store_true")
    parser.add_argument("--min-criteria", type=float, default=0.90)
    parser.add_argument("--min-fully-correct", type=float, default=0.80)
    parser.add_argument("--min-persisted", type=float, default=1.0)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)
    if args.repetitions < 1:
        parser.error("repetitions must be at least 1")
    for name in ("min_criteria", "min_fully_correct", "min_persisted"):
        value = getattr(args, name)
        if not 0.0 <= value <= 1.0:
            parser.error(f"--{name.replace('_', '-')} must be between 0 and 1")
    return args


ARGS = parse_args(sys.argv[1:])
B = ARGS.base_url.rstrip("/")
K = ARGS.repetitions


def _req(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    # Retry transient timeouts (a live-model turn can briefly stall the event POST).
    for attempt in range(4):
        r = urllib.request.Request(B + path, data=data, method=method,
                                   headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(r, timeout=90) as resp:
                return resp.status, json.loads(resp.read() or "null")
        except urllib.error.HTTPError as e:
            return e.code, None
        except (TimeoutError, urllib.error.URLError):
            if attempt == 3:
                return 0, None
            time.sleep(2)


def draft(agent_id, prompt, timeout=90):
    """Run one assistant session that should author `agent_id`; return its persisted config."""
    _req("DELETE", f"/v1/config/agents/{agent_id}")
    _, s = _req("POST", "/v1/sessions", {"agent": "__admin_assistant", "title": "eval"})
    sid = s["id"]
    _req("POST", f"/v1/sessions/{sid}/events",
         {"events": [{"type": "user.message", "content": [{"type": "text", "text": prompt}]}]})
    deadline = time.time() + timeout
    while time.time() < deadline:
        time.sleep(2)
        _, ev = _req("GET", f"/v1/sessions/{sid}/events")
        types = [e["type"] for e in ev["data"]]
        if "session.error" in types:
            break
        if types and types[-1] == "session.status_idle":
            break
    _, cfg = _req("GET", f"/v1/config/agents/{agent_id}")
    return cfg if cfg and cfg.get("id") == agent_id else None


# ---- golden cases: (id, prompt, [(criterion_name, predicate(config))]) ----
def _system(c):
    # The config-plane agent object carries the system prompt as `system` (the managed
    # identity field); `instructions` is the internal name. Accept either.
    return (c or {}).get("system") or (c or {}).get("instructions") or ""
def has_tool(c, t): return t in (c.get("tools") or [])
def override_for(c, target):
    return next((o for o in (c.get("tool_overrides") or []) if o.get("target") == target), None)
def plugin_cfg(c, pid): return (c.get("plugin_config") or {}).get(pid)

CASES = [
    ("eval-c1",
     "Draft an agent id 'eval-c1' that reviews pull requests. Give it the read and grep tools, "
     "and rename grep to 'search_code' for the model with a helpful description.",
     [("persisted", lambda c: c is not None),
      ("tool:read", lambda c: has_tool(c, "read")),
      ("tool:grep", lambda c: has_tool(c, "grep")),
      ("override:grep→search_code", lambda c: (override_for(c, "grep") or {}).get("alias") == "search_code"),
      ("override:has_description", lambda c: bool((override_for(c, "grep") or {}).get("description")))]),

    ("eval-c2",
     "Draft an agent id 'eval-c2' with the bash and read tools. Require human approval before any "
     "bash command, and always deny shell deletes (rm).",
     [("persisted", lambda c: c is not None),
      ("tool:bash", lambda c: has_tool(c, "bash")),
      ("permission:section", lambda c: plugin_cfg(c, "permission") is not None),
      ("permission:gated", lambda c: _perm_gated(plugin_cfg(c, "permission"))),
      # The discriminating criteria the enriched schema teaches: patterns must use the REAL
      # lowercase tool ids (a `Bash(...)` pattern parses but never matches → latent fail-open),
      # and the rm-deny must actually target `bash`.
      ("permission:lowercase_ids", lambda c: _perm_ids_lowercase(plugin_cfg(c, "permission"))),
      ("permission:denies_bash_rm", lambda c: _perm_denies_bash_rm(plugin_cfg(c, "permission")))]),

    ("eval-c3",
     "Draft an agent id 'eval-c3' with the read and write tools, and add a state_machine plugin that "
     "requires reading a file before writing it, emitting a system reminder if it writes without reading first.",
     [("persisted", lambda c: c is not None),
      ("plugin:state_machine", lambda c: "state_machine" in (c.get("plugins") or [])),
      ("sm:has_transitions", lambda c: _sm_has_transitions(plugin_cfg(c, "state_machine"))),
      ("sm:has_reminder", lambda c: _sm_has_reminder(plugin_cfg(c, "state_machine")))]),

    ("eval-c4",
     "Draft an agent id 'eval-c4' for long research sessions. Enable auto-compaction with a custom "
     "compaction prompt that keeps key findings and open questions.",
     [("persisted", lambda c: c is not None),
      ("plugin:compact", lambda c: "compact" in (c.get("plugins") or [])),
      ("compact:has_instructions", lambda c: bool(_compact_instructions(plugin_cfg(c, "compact")))),
      # The enriched schema teaches that `instructions` is a PROMPT (prose, not a size) and
      # that the trigger is `trigger_ratio` of the window — check the weak model got both.
      ("compact:instructions_is_prose", lambda c: _compact_prose(plugin_cfg(c, "compact"))),
      ("compact:trigger_ratio_set", lambda c: _compact_ratio(plugin_cfg(c, "compact")))]),

    ("eval-c5",
     "Draft an agent id 'eval-c5': a terse assistant that writes haiku. No tools needed.",
     [("persisted", lambda c: c is not None),
      ("instructions:nontrivial", lambda c: len(_system(c)) > 40),
      ("tools:empty", lambda c: not (c.get("tools") if c else True))]),
]


def _perm_gated(p):
    if not isinstance(p, dict): return False
    if p.get("default_behavior") in ("ask", "deny"): return True
    return any(r.get("behavior") in ("ask", "deny") for r in (p.get("rules") or []))

def _sm_has_transitions(sm):
    return isinstance(sm, dict) and bool(sm.get("transitions") or
        any(isinstance(m, dict) and m.get("transitions") for m in (sm.get("machines") or [])))

def _sm_has_reminder(sm):
    # A reminder is any emit/message string anywhere in the state machine config.
    blob = json.dumps(sm) if sm else ""
    return any(k in blob for k in ("message", "emit", "reminder"))

def _compact_instructions(cc):
    if not isinstance(cc, dict): return None
    return cc.get("instructions") or cc.get("prompt")

# Real lowercase tool ids (the built-ins). A rule pattern's head must be one of these — a
# capitalized `Bash`/`Read` parses but never matches the runtime tool (silent fail-open).
_REAL_TOOL_IDS = {"bash", "read", "write", "edit", "glob", "grep"}

def _rule_patterns(p):
    return [r.get("pattern", "") for r in (p.get("rules") or [])] if isinstance(p, dict) else []

def _pattern_head(pat):
    # `bash(command ~ "*rm*")` → `bash`; `*` and `mcp__x__*` pass through.
    return pat.split("(")[0].strip()

def _perm_ids_lowercase(p):
    heads = [_pattern_head(x) for x in _rule_patterns(p)]
    heads = [h for h in heads if h and h != "*" and not h.startswith("mcp__")]
    # Non-empty, and every named-tool head is a real lowercase id (rejects `Bash`, `Read`).
    return bool(heads) and all(h in _REAL_TOOL_IDS for h in heads)

def _perm_denies_bash_rm(p):
    if not isinstance(p, dict): return False
    for r in (p.get("rules") or []):
        pat = r.get("pattern", "")
        if r.get("behavior") == "deny" and _pattern_head(pat) == "bash" and "rm" in pat:
            return True
    return False

def _compact_prose(cc):
    s = _compact_instructions(cc)
    # A prompt is prose: a phrase with spaces, not a bare number/size the field isn't for.
    return isinstance(s, str) and len(s) > 20 and " " in s.strip() and not s.strip().isdigit()

def _compact_ratio(cc):
    if not isinstance(cc, dict): return False
    r = cc.get("trigger_ratio")
    return isinstance(r, (int, float)) and 0 < r <= 1


def gate_failures(grand, args):
    """Return stable, human-readable quality-floor failures."""
    rates = {
        "criteria": grand["crit_pass"] / grand["crit_total"],
        "fully-correct": grand["case_full"] / grand["case_runs"],
        "persisted": grand["persist"] / grand["case_runs"],
    }
    floors = {
        "criteria": args.min_criteria,
        "fully-correct": args.min_fully_correct,
        "persisted": args.min_persisted,
    }
    return [
        f"{name} rate {rates[name]:.3f} is below required {floor:.3f}"
        for name, floor in floors.items()
        if rates[name] < floor
    ]


def self_test():
    thresholds = argparse.Namespace(
        min_criteria=0.90,
        min_fully_correct=0.80,
        min_persisted=1.0,
    )
    passing = {
        "crit_pass": 9, "crit_total": 10,
        "case_full": 4, "case_runs": 5, "persist": 5,
    }
    assert gate_failures(passing, thresholds) == []
    failing = dict(passing, crit_pass=8, case_full=3, persist=4)
    assert [failure.split(" rate", 1)[0] for failure in gate_failures(failing, thresholds)] == [
        "criteria", "fully-correct", "persisted"
    ]
    print("OK - assistant eval gate thresholds fail closed")
    return 0


def main():
    if ARGS.self_test:
        return self_test()
    # Optional case-id filter (e.g. `eval-c2,eval-c4`).
    only = set(ARGS.cases.split(",")) if ARGS.cases else None
    cases = [c for c in CASES if not only or c[0] in only]
    print(f"# Admin Assistant authoring skill — eval (K={K} reps/case)\n")
    grand = {"crit_pass": 0, "crit_total": 0, "case_full": 0, "case_runs": 0, "persist": 0}
    for cid, prompt, rubric in cases:
        # per-criterion pass counts across reps → consistency
        counts = {name: 0 for name, _ in rubric}
        full = 0
        for _ in range(K):
            cfg = draft(cid, prompt)
            grand["persist"] += 1 if cfg is not None else 0
            all_ok = True
            for name, pred in rubric:
                ok = False
                try:
                    ok = bool(pred(cfg))
                except Exception:
                    ok = False
                counts[name] += 1 if ok else 0
                all_ok = all_ok and ok
            full += 1 if all_ok else 0
        print(f"## {cid}")
        for name, _ in rubric:
            rate = counts[name] / K
            bar = "█" * round(rate * 10) + "░" * (10 - round(rate * 10))
            print(f"   {bar} {rate*100:3.0f}%  {name}")
        print(f"   → all-criteria-pass: {full}/{K}  (consistency)\n")
        grand["crit_pass"] += sum(counts.values())
        grand["crit_total"] += len(rubric) * K
        grand["case_full"] += full
        grand["case_runs"] += K
    print("# TOTALS")
    print(f"   criteria pass rate : {grand['crit_pass']}/{grand['crit_total']} "
          f"= {100*grand['crit_pass']/grand['crit_total']:.0f}%")
    print(f"   fully-correct runs : {grand['case_full']}/{grand['case_runs']} "
          f"= {100*grand['case_full']/grand['case_runs']:.0f}%")
    print(f"   persisted (compiled): {grand['persist']}/{grand['case_runs']} "
          f"= {100*grand['persist']/grand['case_runs']:.0f}%")
    if not ARGS.gate:
        return 0
    failures = gate_failures(grand, ARGS)
    if failures:
        print("\n# GATE FAILED", file=sys.stderr)
        for failure in failures:
            print(f"   - {failure}", file=sys.stderr)
        return 1
    print("\n# GATE PASSED")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
