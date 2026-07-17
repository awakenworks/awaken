#!/usr/bin/env python3
"""Runtime-prompt eval harness — the family-B counterpart of assistant-eval.py.

assistant-eval scores STRUCTURED CONFIG the assistant authors. This scores the
OUTPUT QUALITY of the runtime sub-agent prompts a plugin drives — compaction,
memory extraction/selection, and the outcome judge — on the WEAKEST model, over
golden fixtures with known-correct answers, K times each for CONSISTENCY.

Method (faithful reproduction): each sub-agent IS just an agent with fixed
instructions + tools + max_steps (see ext-compact/agent.rs, ext-memory/agent.rs,
runtime-host/judge.rs). We recreate that agent on the config plane with the prompt
UNDER TEST, feed it the golden input, and score its reply against a rubric or
ground truth. Swap BASELINE→CANDIDATE prompts to A/B an optimization.

Metrics (the KPI per prompt — ground truth or rubric, scored 0..1, averaged over K reps):
  selector  exact       — picked index set == the relevant set (strict)
            precision   — of the picked, fraction relevant (the SAFETY axis: never inject a
                          wrong memory) ;  recall — of the relevant, fraction picked
  judge     valid_json  — reply parses to {result in [satisfied, needs_revision]}
            correct     — the verdict matches the known-correct grade (calibration)
  compact   preserve_recall — fraction of must-keep durable facts present in the summary
                              (the SAFETY axis: never lose context)
            drop_rate       — fraction of must-drop items (small talk, SUPERSEDED decisions,
                              COMPLETED work) correctly omitted
  extract   types_covered   — every expected memory type was saved
            skipped_forbidden — no forbidden content (code / file paths / fix recipes) saved

Method note: the weakest model is a STRESS FLOOR, not the target — a prompt tuned only to
satisfy it can over-constrain capable models. Optimize for robustness across the range; use
adversarial golden cases (reversed decisions, off-by-one, count violations) so a "100%" is
real, not easy-case luck. Levers, weakest→strongest: wording < few-shot example < structural
output constraint (a code post-filter) < decomposition < model routing.

Usage: python3 runtime-prompt-eval.py [K] [which] [--cand file.json]
  K     reps/case (default 3);  which = selector|judge|compact|extract|all (default all)
Env:  KIMI_KEY (weakest model moonshot-v1-8k, via the config plane — no server env)
"""
import json, re, sys, time, urllib.request, urllib.error

B = "http://127.0.0.1:38080"
MODEL = "moonshot-v1-8k"  # the WEAKEST KIMI model — the consistency floor
K = int(sys.argv[1]) if len(sys.argv) > 1 else 3
WHICH = sys.argv[2] if len(sys.argv) > 2 else "all"
KIMI = __import__("os").environ.get("KIMI_KEY", "")


def _req(method, path, body=None, timeout=90):
    data = json.dumps(body).encode() if body is not None else None
    for attempt in range(4):
        r = urllib.request.Request(B + path, data=data, method=method,
                                   headers={"content-type": "application/json"})
        try:
            with urllib.request.urlopen(r, timeout=timeout) as resp:
                return resp.status, json.loads(resp.read() or "null")
        except urllib.error.HTTPError as e:
            return e.code, None
        except (TimeoutError, urllib.error.URLError):
            if attempt == 3:
                return 0, None
            time.sleep(2)


def configure_kimi():
    _req("PUT", "/v1/config/providers/kimi", {"id": "kimi", "slug": "kimi", "display_name": "Kimi", "version": 1})
    _req("PUT", "/v1/config/endpoints/kimi-ep", {"id": "kimi-ep", "provider_id": "kimi", "dialect": "anthropic_messages",
         "base_url": "https://api.kimi.com/coding/v1/", "timeout_secs": 60, "display_name": "Kimi", "version": 1})
    _req("POST", "/v1/config/offerings", {"model_id": MODEL, "provider_id": "kimi",
         "protocol_endpoint_id": "kimi-ep", "dialect": "anthropic_messages", "upstream_model": None})
    _req("POST", "/v1/config/credentials", {"workspace_id": "wrkspc_default", "kind": "vault",
         "provider_id": "kimi", "secret": KIMI})


def run_agent(agent_id, instructions, user_text, *, tools=None, plugins=None, max_steps=2, timeout=90):
    """Publish an agent that reproduces a sub-agent (instructions+tools+max_steps),
    drive it with `user_text`, and return (final_text, tool_calls) from the run."""
    _req("DELETE", f"/v1/config/agents/{agent_id}")
    cfg = {"id": agent_id, "model": {"id": MODEL}, "system": instructions, "tools": tools or [],
           "plugins": plugins or [], "context_policy": {"kind": "keep_all"}, "max_steps": max_steps}
    _req("PUT", f"/v1/config/agents/{agent_id}", cfg)
    _req("POST", f"/v1/config/agents/{agent_id}/publish")
    _, s = _req("POST", "/v1/sessions", {"agent": agent_id, "title": "rt-eval"})
    if not s:
        return "", []
    sid = s["id"]
    _req("POST", f"/v1/sessions/{sid}/events",
         {"events": [{"type": "user.message", "content": [{"type": "text", "text": user_text}]}]})
    deadline = time.time() + timeout
    while time.time() < deadline:
        time.sleep(2)
        _, ev = _req("GET", f"/v1/sessions/{sid}/events")
        types = [e["type"] for e in (ev or {}).get("data", [])]
        if "session.error" in types or (types and types[-1] == "session.status_idle"):
            break
    _, ev = _req("GET", f"/v1/sessions/{sid}/events")
    data = (ev or {}).get("data", [])
    text, calls = "", []
    for e in data:
        if e.get("type") == "agent.message":
            text = "".join(c.get("text", "") for c in (e.get("content") or []) if isinstance(c, dict))
        if e.get("type") in ("agent.tool_use", "agent.custom_tool_use"):
            for c in (e.get("content") or []):
                if isinstance(c, dict) and (c.get("name") or c.get("input") is not None):
                    calls.append({"name": c.get("name"), "input": c.get("input")})
    return text, calls


# ── BASELINE prompts (verbatim from the crates; swap for a CANDIDATE to A/B) ──────
BASELINE = {
    "selector": (
        "You select which of a user's saved memories are relevant to their current message. "
        "Reply with ONLY the bracketed indices of the relevant memories (e.g. `[0], [3]`), "
        "comma-separated, at most the requested count. If none are relevant, reply NONE. "
        "Do not explain, do not use tools."
    ),
    "judge": (
        "You are a strict evaluator. You are given a goal, its rubric, and a deliverable. "
        "Judge whether the deliverable satisfies the rubric. Reply with ONLY a JSON object "
        "of the form {\"result\": \"satisfied\" | \"needs_revision\", \"explanation\": \"...\"} "
        "and nothing else."
    ),
    "compact": (  # mirrors ext-compact DEFAULT_COMPACT_INSTRUCTIONS (optimized: drop 65→88%)
        "You are a conversation-compaction sub-agent. Summarize the earlier conversation into "
        "the durable facts a continuation needs: decisions still in force, active constraints, "
        "UNRESOLVED open questions, and results that still matter. Preserve every such fact — do "
        "not lose them.\n\nTwo hard DROP rules:\n1. If a decision was later changed or reversed, "
        "keep ONLY the final choice — never mention the superseded one.\n2. If work was already "
        "completed and needs no follow-up, leave it out — never restate a finished fix or task as "
        "if it were still pending.\nAlso drop small talk and any line that would not change what "
        "the next turn does.\n\nReply with only the summary text."
    ),
    "extract": (
        "You are the memory extraction sub-agent. Analyze the conversation you are given "
        "and update a persistent memory so future conversations understand who the user "
        "is, how they want you to work, and the context behind their tasks.\n\n"
        "## Types of memory to save\n"
        "- user: the user's role, goals, responsibilities, preferences, and knowledge.\n"
        "- feedback: guidance on how to approach work — corrections AND confirmations.\n"
        "- project: ongoing work, goals, decisions, or incidents not derivable from code/git.\n"
        "- reference: pointers to where information lives in external systems.\n\n"
        "## What NOT to save\n"
        "- Code patterns, conventions, architecture, file paths, project structure.\n"
        "- Git history or who-changed-what.\n"
        "- Debugging solutions or fix recipes.\n"
        "- Ephemeral task state or anything trivial or easily re-derived.\n\n"
        "## How to save\n"
        "Save each memory with the write_memory tool: a short kebab-case slug name and the "
        "memory text. Prefer one memory per distinct fact."
    ),
}
# Candidate prompts loaded from a JSON file (--cand) override the baseline per key.
PROMPTS = dict(BASELINE)
for i, a in enumerate(sys.argv):
    if a == "--cand" and i + 1 < len(sys.argv):
        PROMPTS.update(json.load(open(sys.argv[i + 1])))

# The extract eval captures the SAVE DECISION (taxonomy + what-to-skip — the quality
# lever) as JSON, since write_memory is a plugin-internal tool a config agent can't name;
# the tool mechanics are covered by the ext-memory Rust tests.
EXTRACT_REDIRECT = ("\n\n## For THIS run only\n"
                    "Do not call any tool. Instead reply with ONLY a JSON array of the memories "
                    "you would save, each {\"type\": \"user|feedback|project|reference\", "
                    "\"name\": \"<slug>\", \"content\": \"<text>\"}. Empty array [] if nothing.")


# ── Golden fixtures + scorers ─────────────────────────────────────────────────────
def manifest(mems, query, n):
    lines = "\n".join(f"[{i}] {m}" for i, m in enumerate(mems))
    return f"Saved memories:\n{lines}\n\nUser message: {query}\n\nReply with the bracketed indices of at most {n} relevant memories."

SELECTOR_CASES = [
    # (mems, query, n, ground-truth relevant set)
    ([ "The user is a Rust backend engineer who prefers terse code review.",
       "The user's timezone is UTC+8 and they work late.",
       "Deploys go through the staging cluster first, never straight to prod.",
       "The user dislikes emoji in commit messages." ],
     "Can you review this PR and keep the feedback short?", 2, {0, 3}),
    ([ "The API base url for the billing service is bill.internal.",
       "The user is allergic to peanuts.",
       "CI must stay green before any merge; the user enforces this strictly." ],
     "Why did my merge get blocked?", 2, {2}),
    ([ "The user writes documentation in British English.",
       "The design doc lives in Notion under 'Platform 2.0'.",
       "The user is learning Japanese." ],
     "Where's the platform redesign spec?", 2, {1}),
]

def score_selector(reply, gt, n):
    picked = set(parse_indices_py(reply, 999, n))
    exact = picked == gt
    tp = len(picked & gt)
    prec = tp / len(picked) if picked else (1.0 if not gt else 0.0)
    rec = tp / len(gt) if gt else 1.0
    return {"exact": exact, "precision": prec, "recall": rec}

def parse_indices_py(reply, nmax, cap):
    if reply.strip().upper().startswith("NONE"):
        return []
    nums = [int(x) for x in re.findall(r"\d+", reply)]
    out = []
    for x in nums:
        if x < nmax and x not in out:
            out.append(x)
    return out[:cap]

JUDGE_CASES = [
    ("Write a function that returns the nth Fibonacci number.",
     "Correct for n=0..10; handles n=0 → 0.",
     "def fib(n):\n  a,b=0,1\n  for _ in range(n): a,b=b,a+b\n  return a", "satisfied"),
    ("Write a function that returns the nth Fibonacci number.",
     "Correct for n=0..10; handles n=0 → 0.",
     "def fib(n):\n  return n  # TODO", "needs_revision"),
    ("Summarize the quarterly report in 3 bullet points.",
     "Exactly 3 bullets, each one sentence, covers revenue/costs/outlook.",
     "- Revenue rose 12%.\n- Costs held flat.\n- Outlook is positive for Q4.", "satisfied"),
    ("Summarize the quarterly report in 3 bullet points.",
     "Exactly 3 bullets, each one sentence, covers revenue/costs/outlook.",
     "The quarter went well overall with strong numbers.", "needs_revision"),
    # --- adversarial: plausible-but-wrong, count-violation, correct-but-terse, off-by-one ---
    ("Write a function that returns the nth Fibonacci number.",
     "Correct for n=0..10; handles n=0 → 0.",
     # Looks right but is off-by-one (returns fib(n+1)); must NOT be waved through.
     "def fib(n):\n  a,b=0,1\n  for _ in range(n+1): a,b=b,a+b\n  return a", "needs_revision"),
    ("Summarize the quarterly report in 3 bullet points.",
     "Exactly 3 bullets, each one sentence, covers revenue/costs/outlook.",
     # 4 bullets — violates the explicit count even though content is fine.
     "- Revenue rose 12%.\n- Costs held flat.\n- Outlook positive.\n- Hiring continues.", "needs_revision"),
    ("List three prime numbers.",
     "Exactly three numbers, all prime.",
     "2, 3, 5", "satisfied"),
    ("List three prime numbers.",
     "Exactly three numbers, all prime.",
     "2, 3, 9", "needs_revision"),  # 9 is not prime — subtle
]

def score_judge(reply, expected):
    m = re.search(r"\{.*\}", reply, re.DOTALL)
    if not m:
        return {"valid_json": False, "correct": False}
    try:
        obj = json.loads(m.group(0))
    except Exception:
        return {"valid_json": False, "correct": False}
    result = str(obj.get("result", "")).strip()
    valid = result in ("satisfied", "needs_revision")
    return {"valid_json": valid, "correct": valid and result == expected}

COMPACT_CASES = [
    # (conversation, must-preserve substrings, must-drop substrings)
    ("User: Let's build the export feature. Decision: we'll use CSV, not JSON, for v1.\n"
     "Assistant: Got it. Any constraints?\n"
     "User: Yes — it must stream, files can be 2GB. Also the deadline is Friday.\n"
     "Assistant: Noted. By the way, nice weather today!\n"
     "User: haha yeah. Open question: do we paginate the API or stream raw?",
     ["CSV", "2GB", "Friday", "paginate"], ["weather"]),
    ("User: The bug is in the auth middleware — tokens expire too early.\n"
     "Assistant: I fixed it by extending the TTL to 3600s. Tests pass now.\n"
     "User: Great. Next: we still need to handle refresh tokens, that's unresolved.",
     ["refresh token", "auth"], ["Tests pass"]),
    # --- adversarial: a REVERSED decision — must keep the FINAL choice, drop the superseded one ---
    ("User: Let's use MongoDB for the store.\n"
     "Assistant: Okay.\n"
     "User: Actually, scratch that — we're going with Postgres instead, final decision.\n"
     "Assistant: Understood, Postgres it is.\n"
     "User: Also the migration must finish before the March 3 launch.",
     ["Postgres", "March 3"], ["MongoDB"]),
    # --- adversarial: many facts, only two durable; the rest is resolved noise ---
    ("User: The dashboard is slow.\n"
     "Assistant: I profiled it and added an index; it's fast now, done.\n"
     "User: Good. Constraint going forward: every query must stay under 100ms.\n"
     "Assistant: Noted.\n"
     "User: Open item: we still need to decide the caching layer — Redis vs in-memory.",
     ["100ms", "caching"], ["profiled", "added an index"]),
]

def score_compact(summary, preserve, drop):
    s = summary.lower()
    kept = sum(1 for k in preserve if k.lower() in s)
    dropped = sum(1 for d in drop if d.lower() not in s)
    return {"preserve_recall": kept / len(preserve), "drop_rate": dropped / len(drop) if drop else 1.0}

EXTRACT_CASES = [
    # (conversation, expected saved types (>=1 each), forbidden substrings that must NOT be saved)
    ("User: I'm the lead SRE for the payments team. Please always run terraform plan before apply.\n"
     "Assistant: Understood.\n"
     "User: Also, the incident runbook is in Confluence under 'Payments Oncall'.",
     {"user", "feedback", "reference"}, ["terraform apply", "def "]),
    ("User: We decided to migrate off Redis to Postgres for the job queue — locked in last sprint.\n"
     "Assistant: I refactored queue.rs to use a pg table; the fix was a missing index.\n"
     "User: Good.",
     {"project"}, ["queue.rs", "missing index"]),
]

def score_extract(reply, expected_types, forbidden):
    m = re.search(r"\[.*\]", reply, re.DOTALL)
    saved = []
    if m:
        try:
            saved = json.loads(m.group(0))
        except Exception:
            saved = []
    types = {str(x.get("type", "")).lower() for x in saved if isinstance(x, dict)}
    blob = json.dumps(saved).lower()
    types_ok = expected_types.issubset(types)
    no_forbidden = all(f.lower() not in blob for f in forbidden)
    return {"types_covered": types_ok, "skipped_forbidden": no_forbidden, "count": len(saved)}


# ── Runners: one per family, K reps → consistency ────────────────────────────────
def bar(rate):
    return "█" * round(rate * 10) + "░" * (10 - round(rate * 10))

def run_family(name, cases, run_one, crit_names):
    print(f"## {name}  (prompt: {'CANDIDATE' if PROMPTS[name] != BASELINE[name] else 'baseline'})")
    counts = {c: 0.0 for c in crit_names}
    reps = 0
    for ci, case in enumerate(cases):
        for _ in range(K):
            scores = run_one(f"rt-{name}-{ci}", case)
            for c in crit_names:
                counts[c] += float(scores.get(c, 0.0))
            reps += 1
    for c in crit_names:
        rate = counts[c] / reps if reps else 0.0
        print(f"   {bar(rate)} {rate*100:3.0f}%  {c}")
    print()
    return {c: counts[c] / reps if reps else 0.0 for c in crit_names}


def one_selector(aid, case):
    mems, query, n, gt = case
    text, _ = run_agent(aid, PROMPTS["selector"], manifest(mems, query, n), max_steps=1)
    return score_selector(text, gt, n)

def one_judge(aid, case):
    goal, rubric, deliverable, expected = case
    prompt = f"Goal: {goal}\nRubric: {rubric}\nDeliverable:\n{deliverable}"
    text, _ = run_agent(aid, PROMPTS["judge"], prompt, max_steps=2)
    return score_judge(text, expected)

def one_compact(aid, case):
    convo, preserve, drop = case
    # The real compactor gets the older slice + SUMMARIZE_PROMPT.
    user = f"{convo}\n\nSummarize the conversation above per your instructions."
    text, _ = run_agent(aid, PROMPTS["compact"], user, max_steps=2)
    return score_compact(text, preserve, drop)

def one_extract(aid, case):
    convo, types, forbidden = case
    text, _ = run_agent(aid, PROMPTS["extract"] + EXTRACT_REDIRECT,
                        f"{convo}\n\nExtract durable memories from the conversation above.", max_steps=2)
    return score_extract(text, types, forbidden)


FAMILIES = {
    "selector": (SELECTOR_CASES, one_selector, ["exact", "precision", "recall"]),
    "judge": (JUDGE_CASES, one_judge, ["valid_json", "correct"]),
    "compact": (COMPACT_CASES, one_compact, ["preserve_recall", "drop_rate"]),
    "extract": (EXTRACT_CASES, one_extract, ["types_covered", "skipped_forbidden"]),
}


def main():
    if not KIMI:
        print("set KIMI_KEY"); sys.exit(1)
    configure_kimi()
    print(f"# Runtime-prompt eval — model={MODEL} (weakest), K={K} reps/case\n")
    picks = FAMILIES.keys() if WHICH == "all" else [WHICH]
    for name in picks:
        cases, run_one, crits = FAMILIES[name]
        run_family(name, cases, run_one, crits)


if __name__ == "__main__":
    main()
