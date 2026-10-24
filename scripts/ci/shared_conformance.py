"""The canonical shared-store and dispatch conformance wiring guard."""
from __future__ import annotations

import re

from _crate_boundary_workspace import rust_syntax, _balanced_end
from _cancel_safety_fitness import AWAIT, FN_HEAD, _skip_generics, function_bodies

TEST_ATTRIBUTE = re.compile(
    r"#\[(?:tokio::)?test[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:async\s+)?fn\s+(\w+)"
)


def shared_conformance_errors(
    store_suite: str,
    backend_tests: dict[str, str],
    ingress_tests: str,
) -> list[str]:
    errors: list[str] = []
    # This is the existing suite's wiring projection, not a maintained case
    # list: public declarations own cases, real backend tests own roots, and
    # executable calls within the existing functions own reachability.
    def owned_functions(text: str, label: str) -> tuple[str, dict, set[str]] | None:
        syntax = rust_syntax(text)
        spans = function_bodies(syntax)
        by_start = {item["start"]: item for item in spans}
        for head in FN_HEAD.finditer(syntax):
            cursor = head.end()
            if cursor < len(syntax) and syntax[cursor] == "<":
                if _balanced_end(syntax, cursor, "<", ">") is None:
                    errors.append(f"{label}: cannot parse a shared conformance function declaration")
                    return None
                cursor = _skip_generics(syntax, cursor)
            if cursor >= len(syntax) or syntax[cursor] != "(":
                errors.append(f"{label}: cannot parse a shared conformance function declaration")
                return None
            params_end = _balanced_end(syntax, cursor, "(", ")")
            if params_end is None:
                errors.append(f"{label}: unbalanced shared conformance function parameters")
                return None
            item = by_start.get(head.start())
            if item is None:
                # The original function reader intentionally skips trait
                # declarations ending in ';'; it may not silently skip a body
                # or malformed declaration and turn it into an empty success.
                brace, semicolon = syntax.find("{", params_end), syntax.find(";", params_end)
                if semicolon < 0 or 0 <= brace < semicolon:
                    errors.append(f"{label}: cannot confirm shared conformance function body")
                    return None
            elif _balanced_end(syntax, item["body"], "{", "}") != item["end"] + 1:
                errors.append(f"{label}: unbalanced shared conformance function body")
                return None
        names = [item["name"] for item in spans]
        if len(set(names)) != len(names):
            errors.append(f"{label}: ambiguous shared conformance function names")
            return None
        bodies = {}
        for item in spans:
            # A nested fn declaration is not execution of that function. Keep
            # its independently parsed node, blank its item in its parent body.
            body = list(syntax[item["body"] + 1:item["end"]])
            for child in spans:
                if item["body"] < child["start"] < item["end"]:
                    start = child["start"] - item["body"] - 1
                    end = child["end"] - item["body"]
                    body[start:end] = " " * (end - start)
            bodies[item["name"]] = "".join(body)
        return syntax, bodies, set(re.findall(r"\basync\s+(?:unsafe\s+)?fn\s+([A-Za-z_]\w*)", syntax))

    suite = owned_functions(store_suite, "shared store suite")
    store_cases = re.findall(r"^pub\s+async\s+(?:unsafe\s+)?fn\s+([a-z0-9_]+)", rust_syntax(store_suite), re.MULTILINE)
    if not store_cases or len(set(store_cases)) != len(store_cases):
        errors.append("shared store suite has no unique public conformance cases")
    # Call expressions only. Path qualifiers are judged in their own scope;
    # methods, comments and strings cannot impersonate a local or canonical call.
    calls = re.compile(r"(?<![\w.:])((?:[A-Za-z_]\w*\s*::\s*)*)([A-Za-z_]\w*)"
                       r"\s*(?:::\s*<[^;{}]*>)?\s*\(")
    if suite is not None and not set(store_cases) <= set(suite[1]):
        errors.append("shared store suite public cases lack parsed function bodies")
        suite = None
    if not backend_tests:
        errors.append("shared store suite has no backend conformance bindings")
    for relative, test_text in backend_tests.items():
        backend = owned_functions(test_text, relative)
        if suite is None or backend is None:
            continue
        roots = TEST_ATTRIBUTE.findall(backend[0])
        if not roots or len(set(roots)) != len(roots) or not set(roots) <= set(backend[1]):
            errors.append(f"{relative}: no unique parsed backend conformance test roots")
            continue
        pending = [("backend", root) for root in roots]
        reached = set()
        scopes = {"backend": backend[1], "suite": suite[1]}
        asynchronous = {"backend": backend[2], "suite": suite[2]}
        while pending:
            scope, name = pending.pop()
            node = (scope, name)
            if node in reached:
                continue
            reached.add(node)
            for call in calls.finditer(scopes[scope][name]):
                path = re.sub(r"\s", "", call.group(1)).removesuffix("::")
                callee = call.group(2)
                destination = None
                if path in ("", "self", "crate") and callee in scopes[scope]:
                    destination = scope
                elif scope == "backend" and path == "awaken_store_conformance" and callee in store_cases:
                    destination = "suite"
                if destination is None:
                    continue
                close = _balanced_end(scopes[scope][name], call.end() - 1, "(", ")")
                if close is None:
                    errors.append(f"{relative}: unbalanced conformance call expression")
                    continue
                # Creating an async Future is not executing its conformance.
                # The existing AWAIT matcher confirms the actual call is polled.
                # Passing/returning a future through another executor is unknown,
                # never a reason to infer execution or add a second proof path.
                if callee in asynchronous[destination] and AWAIT.match(scopes[scope][name], close) is None:
                    continue
                pending.append((destination, callee))
        missing = [case for case in store_cases if ("suite", case) not in reached]
        if missing:
            errors.append(f"{relative} omits shared store cases: " + ", ".join(missing))

    # The ingress testkit intentionally owns no backend. Its executable proof is
    # the same suite applied to each production implementation.
    ingress = owned_functions(ingress_tests, "dispatch conformance")
    if ingress is None:
        return errors
    roots = set(TEST_ATTRIBUTE.findall(ingress[0]))
    invalid = {"conformance": [], "operational-feed": []}
    for backend in ("memory", "sqlite", "postgres"):
        root = f"{backend}_dispatch_conforms"
        bindings = {family: set() for family in invalid}
        if root not in roots or root not in ingress[1]:
            errors.append(f"dispatch conformance omits {backend} backend")
        else:
            pending, reached = [root], set()
            while pending:
                name = pending.pop()
                if name in reached:
                    continue
                reached.add(name)
                body = ingress[1][name]
                for call in calls.finditer(body):
                    path = re.sub(r"\s", "", call.group(1)).removesuffix("::")
                    callee = call.group(2)
                    close = _balanced_end(body, call.end() - 1, "(", ")")
                    if close is None:
                        errors.append("dispatch conformance has an unbalanced call")
                        continue
                    awaited = AWAIT.match(body, close) is not None
                    if path in ("", "self", "crate") and callee in ingress[1]:
                        if callee not in ingress[2] or awaited:
                            pending.append(callee)
                    elif path in ("", "awaken_dispatch_testkit") and awaited:
                        boundary = re.fullmatch(
                            r"assert_dispatch_(operational_feed_)?conformance(?:_with_clock)?", callee,
                        )
                        if boundary:
                            family = "operational-feed" if boundary[1] else "conformance"
                            bindings[family].add((name, call.start()))
        for family, bound in bindings.items():
            if len(bound) != 1:
                invalid[family].append(f"{backend}={len(bound)}")
    for family, backends in invalid.items():
        if backends:
            errors.append(f"dispatch {family} suite is not applied to exactly three backends: "
                          + ", ".join(backends))
    return errors




def self_test(store_suite: str, backend_tests: dict[str, str], ingress_tests: str) -> None:
    """The original R1/R8/C7 tests follow their single conformance owner."""
    assert not shared_conformance_errors(store_suite, backend_tests, ingress_tests), "R1/C7"
    # R1/R8 cause/effect for one canonical suite: public declarations own
    # cases; real #[test] functions own backend roots. Calls (not substrings)
    # may reach a public helper through a warm case/private wrapper. Removing
    # either actual edge must refuse. Comments/rawstrings/methods/other namespace
    # and an orphan/nested declaration are not execution. The graph's scoped
    # nodes keep same-named backend wrappers from impersonating suite functions.
    # | Rule | suite/backend call path                    | effect          |
    # | R16  | direct, local private, nested warm helper  | covered         |
    # | R17  | remove actual edge or wrong namespace     | missing case    |
    # | R18  | rooted cycle / orphan cycle               | covered/missing |
    # | R19  | empty/duplicate/no roots/unbalanced       | refusal         |
    # Async call edges must be polled via the existing immediate AWAIT matcher:
    # helper() or a discarded Future is not executable conformance. The real
    # Suite/backend edges all use immediate await; other executors stay unknown.
    # R8's detached canonical entry and C7/three-ingress/feed rules remain below.
    nested_suite = ("pub async fn entry() { helper().await; }\n"
                    "pub async fn helper() {}\n")
    nested_backend = ("#[tokio::test]\nasync fn entry() { wrapper().await; }\n"
                      "async fn wrapper() { awaken_store_conformance::entry().await; }\n")
    assert not shared_conformance_errors(nested_suite, {"fixture.rs": nested_backend}, ingress_tests), "R16 nested calls"
    direct_backend = nested_backend.replace("wrapper().await", "awaken_store_conformance::entry().await")
    assert not shared_conformance_errors(nested_suite, {"fixture.rs": direct_backend}, ingress_tests), "R16 direct calls"
    for lost_edge in (
        nested_suite.replace("helper().await;", "helper();"),
        nested_suite.replace("helper().await;", "let unused = helper(); drop(unused);"),
        nested_suite.replace("helper().await;", ""),
        nested_suite.replace("helper().await;", "detached::helper().await;"),
        nested_suite.replace("helper().await;", "object.helper().await;"),
        nested_suite.replace("helper().await;", '// helper().await;\n'),
        nested_suite.replace("helper().await;", 'let note = r#"helper().await;"#;'),
        nested_suite.replace("helper().await;", "async fn unused() { helper().await; }"),
    ):
        assert any("omits shared store cases: helper" in error for error in shared_conformance_errors(
            lost_edge, {"fixture.rs": nested_backend}, ingress_tests)), "R17 helper edge lost"
    unsafe_future_suite = ("pub async fn entry() { private(); }\n"
                           "async unsafe fn private() { helper().await; }\n"
                           "pub async fn helper() {}\n")
    assert shared_conformance_errors(unsafe_future_suite, {"fixture.rs": nested_backend}, ingress_tests), "R17 unpolled async unsafe helper"
    orphan_backend = nested_backend.replace("wrapper().await;", "")
    assert any("omits shared store cases:" in error for error in shared_conformance_errors(
        nested_suite, {"fixture.rs": orphan_backend}, ingress_tests)), "R17 orphan backend wrapper"
    for declaration_call in (
        "let unused = awaken_store_conformance::entry(); drop(unused);",
        "async fn unused() { awaken_store_conformance::entry().await; }",
        'let text = "awaken_store_conformance::entry()";',
        '// awaken_store_conformance::entry();\n',
    ):
        hidden_backend = "#[tokio::test]\nasync fn root() { " + declaration_call + " }\n"
        assert shared_conformance_errors(nested_suite, {"fixture.rs": hidden_backend}, ingress_tests), "R17 noncall root"
    cycle_suite = ("pub async fn entry() { private().await; }\n"
                   "async fn private() { entry().await; helper().await; }\n"
                   "pub async fn helper() {}\n")
    assert not shared_conformance_errors(cycle_suite, {"fixture.rs": nested_backend}, ingress_tests), "R18 rooted cycle"
    assert shared_conformance_errors(cycle_suite, {"fixture.rs": orphan_backend}, ingress_tests), "R18 orphan cycle"
    for broken_suite, broken_backend in (
        ("", nested_backend),
        (nested_suite + nested_suite, nested_backend),
        (nested_suite, nested_backend + nested_backend),
        (nested_suite, nested_backend.replace("#[tokio::test]", "")),
        (nested_suite.removesuffix("}\n"), nested_backend),
        (nested_suite + "async fn malformed( {", nested_backend),
        (nested_suite, nested_backend + "async fn malformed<T {"),
    ):
        assert shared_conformance_errors(broken_suite, {"fixture.rs": broken_backend}, ingress_tests), "R19 unknown shape"
    assert shared_conformance_errors(nested_suite, {}, ingress_tests), "R19 missing all backends"
    first_backend = next(iter(backend_tests))
    detached_backend = dict(backend_tests)
    detached_backend[first_backend] = detached_backend[
        first_backend
    ].replace("awaken_store_conformance::commit_then_read", "detached::commit_then_read")
    assert any(
        "omits shared store cases: commit_then_read" in error
        for error in shared_conformance_errors(
            store_suite, detached_backend, ingress_tests
        )
    ), "R8 store testkit"
    assert any(
        "omits postgres backend" in error
        for error in shared_conformance_errors(
            store_suite,
            backend_tests,
            ingress_tests.replace("async fn postgres_dispatch_conforms", "async fn detached"),
        )
    ), "R8 ingress testkit"

    # Operational-feed binding cause/effect design:
    # C13=a production backend uses the canonical default-clock entrypoint;
    # C14=a production backend uses the canonical explicit-clock entrypoint.
    # E13=both spellings count as one binding to the same shared suite. R14
    # rejects a missing binding; R15 rejects a duplicate binding, regardless of
    # which spelling introduced it. This keeps clock policy orthogonal to suite
    # ownership and preserves the exactly-three-backends invariant.
    assert not shared_conformance_errors(
        store_suite, backend_tests, ingress_tests
    ), "R1/C13-C14 mixed operational-feed clocks"
    # Derive one whole awaited binding from the actual syntax. A literal API
    # spelling silently stopped mutating this fixture when PostgreSQL adopted
    # its observed store clock. Nested clock arguments and formatting belong to
    # the existing balanced-expression reader, not a second source template.
    syntax = rust_syntax(ingress_tests)
    binding = next(re.finditer(
        r"(?<![\w.:])(?:[A-Za-z_]\w*\s*::\s*)*"
        r"assert_dispatch_operational_feed_conformance(?:_with_clock)?\s*\(", syntax,
    ))
    close = _balanced_end(syntax, binding.end() - 1, "(", ")")
    awaited = AWAIT.match(syntax, close) if close is not None else None
    assert awaited is not None, "operational-feed fixture needs an awaited binding"
    semicolon = re.match(r"\s*;", syntax[awaited.end():])
    assert semicolon is not None, "operational-feed fixture needs a complete statement"
    before = ingress_tests[:binding.start()]
    after = ingress_tests[awaited.end() + semicolon.end():]
    missing_operational_feed = before + after
    assert any(
        "operational-feed suite is not applied to exactly three backends" in error
        for error in shared_conformance_errors(
            store_suite, backend_tests, missing_operational_feed
        )
    ), "R14 missing operational-feed binding"
    default_binding = 'assert_dispatch_operational_feed_conformance(&store, "fixture").await;'
    clock_binding = 'assert_dispatch_operational_feed_conformance_with_clock(&store, "fixture", &set_clock).await;'
    for variant in (default_binding, clock_binding):
        assert not shared_conformance_errors(
            store_suite, backend_tests, before + variant + after
        ), "R1/C13-C14 either clock spelling binds the same suite"
    duplicate_operational_feed = before + default_binding + clock_binding + after
    assert any(
        "operational-feed suite is not applied to exactly three backends" in error
        for error in shared_conformance_errors(
            store_suite, backend_tests, duplicate_operational_feed
        )
    ), "R15 duplicate operational-feed binding"
    # Text is not execution; a counted call on another backend cannot fill the
    # missing backend's obligation. Unpolled Futures and foreign namespaces are
    # independent causes, even when all three test functions remain present.
    for imitation in (
        "// " + default_binding + "\n",
        'let display = r#"' + default_binding + '"#;',
        default_binding.replace(".await", ""),
        "detached::" + default_binding,
    ):
        assert any(
            "operational-feed suite is not applied to exactly three backends" in error
            for error in shared_conformance_errors(store_suite, backend_tests, before + imitation + after)
        ), "R16 non-executing operational-feed imitation"
    owner = next(item["name"] for item in function_bodies(syntax)
                 if item["body"] < binding.start() < item["end"])
    alternate = next(item for item in function_bodies(rust_syntax(missing_operational_feed))
                     if item["name"].endswith("_dispatch_conforms") and item["name"] != owner)
    insertion = alternate["body"] + 1
    misplaced = missing_operational_feed[:insertion] + default_binding + missing_operational_feed[insertion:]
    assert any(
        "operational-feed suite is not applied to exactly three backends" in error
        for error in shared_conformance_errors(store_suite, backend_tests, misplaced)
    ), "R17 duplicate elsewhere cannot replace a backend binding"
    delegated = "async fn delegated() { " + default_binding + (
        'assert_dispatch_conformance(&store, "fixture", capabilities).await; }\n'
    ) + "\n".join("#[tokio::test]\nasync fn " + backend + "_dispatch_conforms() { delegated().await; }"
                   for backend in ("memory", "sqlite", "postgres"))
    assert not shared_conformance_errors(store_suite, backend_tests, delegated), "R18 awaited local delegation"
