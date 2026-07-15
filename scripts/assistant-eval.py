#!/usr/bin/env python3
"""Skill-opt eval harness for the Admin Assistant's agent-authoring skill.

Drives the live assistant (against the running backend on :38080) over a golden set of
natural-language authoring requests, K times each, and scores the PERSISTED draft config
against a per-case rubric. Reports task-success, compile rate, and — the metric that
matters for a structured-authoring skill — CONSISTENCY across repetitions.

Usage: python3 assistant-eval.py [K]        # K reps per case (default 3)
Prints a scorecard; exit 0 always (measurement, not a gate).
"""
import json, sys, time, urllib.request, urllib.error

B = "http://127.0.0.1:38080"
K = int(sys.argv[1]) if len(sys.argv) > 1 else 3


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
      ("permission:gated", lambda c: _perm_gated(plugin_cfg(c, "permission")))]),

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
      ("compact:has_instructions", lambda c: bool(_compact_instructions(plugin_cfg(c, "compact"))))]),

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


def main():
    print(f"# Admin Assistant authoring skill — eval (K={K} reps/case)\n")
    grand = {"crit_pass": 0, "crit_total": 0, "case_full": 0, "case_runs": 0, "persist": 0}
    for cid, prompt, rubric in CASES:
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


if __name__ == "__main__":
    main()
