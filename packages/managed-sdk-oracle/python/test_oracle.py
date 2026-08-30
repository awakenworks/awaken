from __future__ import annotations

import ast
import zipfile
import tempfile
import unittest
from pathlib import Path

import oracle


class PythonOracleExtractorTest(unittest.TestCase):
    def test_extracts_only_sync_transport_and_normalizes_identity(self) -> None:
        # Extractor cause/effect graph: C1=a sync resource contains one HTTP
        # call; C2=its async twin repeats the generated implementation; C3=the
        # route has a named placeholder, beta query, and dated selector.
        # Effects: E1=one snake_case operation is emitted; E2=the async twin is
        # excluded; E3=the wire coordinate is canonical. Decision table:
        # C1+C2+C3=>E1+E2+E3; otherwise extraction must not invent evidence.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "anthropic/resources/beta/memory_stores/memories.py"
            target.parent.mkdir(parents=True)
            target.write_text(
                """
class Memories(SyncAPIResource):
    def retrieve_metadata(self, memory_id):
        return self._get(path_template('/v1/memory_stores/{store_id}/memories/{memory_id}?beta=true', memory_id), headers=['agent-memory-2026-07-22'])

class AsyncMemories(AsyncAPIResource):
    async def retrieve_metadata(self, memory_id):
        return await self._get('/must/not/be/extracted')
""",
                encoding="utf-8",
            )
            operations = oracle.operations_in_file(
                root / "anthropic/resources/beta",
                target,
                "beta",
            )
        self.assertEqual(
            operations,
            [{
                "id": "beta.memory_stores.memories.retrieve_metadata",
                "method": "GET",
                "path": "/v1/memory_stores/{}/memories/{}",
                "betas": ["agent-memory-2026-07-22"],
                "transport_query": "beta=true",
            }],
        )

    def test_rejects_method_with_multiple_transport_calls(self) -> None:
        # Ambiguity partition: a generated method is evidence for exactly one
        # request. Two transport calls cannot be assigned one operation id and
        # therefore fail closed instead of silently selecting the first call.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "anthropic/resources/beta/files.py"
            target.parent.mkdir(parents=True)
            target.write_text(
                """
class Files(SyncAPIResource):
    def broken(self):
        self._get('/v1/files?beta=true')
        return self._post('/v1/files?beta=true')
""",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(AssertionError, "multiple HTTP operations"):
                oracle.operations_in_file(root / "anthropic/resources/beta", target, "beta")

    def test_discovers_handwritten_async_helpers_but_not_subresources_or_operations(self) -> None:
        # Helper-surface partition: C1=an AsyncAPIResource has a handwritten
        # non-transport method; C2=overloads repeat that same helper; C3=a
        # cached subresource accessor has no transport; C4=a generated async
        # operation does. Effects: E1=the helper is recorded once; E2=C3/C4 are
        # excluded. This makes future helper additions/removals oracle-visible.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "anthropic/resources/beta/environments/work.py"
            target.parent.mkdir(parents=True)
            target.write_text(
                """
class AsyncWork(AsyncAPIResource):
    @overload
    def worker(self, *, key: str): ...

    def worker(self, *, key: str):
        return EnvironmentWorker(key)

    async def observe(self):
        return await observe_state()

    @cached_property
    def children(self):
        return Children(self._client)

    async def poll(self):
        return await self._post('/v1/environments/e/work?beta=true')
""",
                encoding="utf-8",
            )
            helpers = oracle.helper_methods_in_file(
                root / "anthropic/resources/beta",
                target,
                "beta",
            )
        self.assertEqual(
            helpers,
            ["beta.environments.work.observe", "beta.environments.work.worker"],
        )

    def test_extracts_only_explicit_static_library_exports(self) -> None:
        # Library-surface partition: C1=the handwritten module owns one static
        # __all__; C2=private/imported names exist beside it. Effect E1=only
        # explicit exports become wheel-bound identities. Dynamic or duplicate
        # declarations fail closed because their compatibility cannot be
        # reviewed deterministically.
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "anthropic/lib/sessions/__init__.py"
            target.parent.mkdir(parents=True)
            target.write_text(
                "from ._accumulate import AccumulatedEvent\n"
                "hidden = object()\n"
                "__all__ = ['AccumulatedEvent', 'accumulate_managed_agents_event']\n",
                encoding="utf-8",
            )
            exports = oracle.public_exports_in_file(target, "anthropic.lib.sessions")
        self.assertEqual(
            exports,
            [
                "anthropic.lib.sessions.AccumulatedEvent",
                "anthropic.lib.sessions.accumulate_managed_agents_event",
            ],
        )

    def test_extracts_exact_sse_dispatch_capabilities(self) -> None:
        # SSE capability graph: C1=the generic parser dispatches one legacy
        # event and two Managed event names; C2=an unrelated string resembles
        # an event. Effects: E1=only actual `sse.event == literal` comparisons
        # become capability evidence; E2=duplicates collapse deterministically.
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "_streaming.py"
            target.write_text(
                """
def decode(sse):
    ignored = 'agent.tool_use'
    if sse.event == 'message' or sse.event == 'agent.message':
        return True
    if sse.event == 'session.status_idle':
        return True
    if sse.event == 'agent.message':
        return True
""",
                encoding="utf-8",
            )
            names = oracle.stream_event_names(target)
        self.assertEqual(names, ["agent.message", "message", "session.status_idle"])

    def test_runtime_source_evidence_fails_closed_on_missing_file(self) -> None:
        # Runtime closure partition: every configured handwritten transport
        # file must exist. Silently skipping a renamed parser would preserve a
        # stale fingerprint while executing unreviewed SDK code.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            existing = root / "anthropic/_streaming.py"
            existing.parent.mkdir(parents=True)
            existing.write_text("pass\n", encoding="utf-8")
            with self.assertRaisesRegex(AssertionError, "missing Python Managed runtime source"):
                oracle.runtime_source_evidence(
                    root,
                    ["anthropic/_streaming.py", "anthropic/_missing.py"],
                )

    def test_python_module_name_accepts_equivalent_root_aliases(self) -> None:
        # macOS reports TemporaryDirectory paths through /var while Path.resolve
        # canonicalizes discovered files through /private/var. A filesystem
        # alias must not change the module identity or bypass containment.
        with tempfile.TemporaryDirectory() as temporary:
            container = Path(temporary)
            real_root = container / "sdk"
            filename = real_root / "anthropic/types/beta/widget.py"
            filename.parent.mkdir(parents=True)
            filename.write_text("class Widget: pass\n", encoding="utf-8")
            alias_root = container / "sdk-alias"
            alias_root.symlink_to(real_root, target_is_directory=True)

            self.assertEqual(
                oracle.python_module_name(alias_root, filename),
                "anthropic.types.beta.widget",
            )

    def test_managed_type_closure_follows_exact_reexported_symbols(self) -> None:
        # Symbol-closure graph: C1=a Managed resource imports one DTO through a
        # broad package initializer; C2=that DTO imports a nested DTO and the
        # common model base; C3=the initializer also re-exports an unrelated
        # Messages DTO. Effects: E1=the exact package/DTO/nested/base files are
        # fingerprinted; E2=C3 is excluded. Mutating any E1 source must change
        # the oracle, while unrelated SDK surfaces cannot inflate the claim.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            resource = root / "anthropic/resources/beta/widgets.py"
            types = root / "anthropic/types/beta"
            resource.parent.mkdir(parents=True)
            types.mkdir(parents=True)
            resource.write_text(
                "from ...types.beta import ManagedThing\n",
                encoding="utf-8",
            )
            (types / "__init__.py").write_text(
                "from .managed_thing import ManagedThing\n"
                "from .message_thing import MessageThing\n",
                encoding="utf-8",
            )
            (types / "managed_thing.py").write_text(
                "from .nested import Nested\nfrom ..._models import BaseModel\n",
                encoding="utf-8",
            )
            (types / "nested.py").write_text("class Nested: pass\n", encoding="utf-8")
            (types / "message_thing.py").write_text(
                "class MessageThing: pass\n",
                encoding="utf-8",
            )
            models = root / "anthropic/_models.py"
            models.write_text("class BaseModel: pass\n", encoding="utf-8")

            evidence = oracle.managed_type_source_evidence(root, [resource])
            first_fingerprint = oracle.digest(evidence)
            (types / "nested.py").write_text(
                "class Nested: changed = True\n",
                encoding="utf-8",
            )
            changed_fingerprint = oracle.digest(
                oracle.managed_type_source_evidence(root, [resource])
            )

        self.assertEqual(
            [item["path"] for item in evidence],
            [
                "anthropic/_models.py",
                "anthropic/types/beta/__init__.py",
                "anthropic/types/beta/managed_thing.py",
                "anthropic/types/beta/nested.py",
            ],
        )
        self.assertNotIn("anthropic/types/beta/message_thing.py", {
            item["path"] for item in evidence
        })
        self.assertNotEqual(first_fingerprint, changed_fingerprint)

    def test_managed_type_closure_classifies_resource_roots_independently_of_traversal_order(self) -> None:
        # Cause/effect graph: C1=two canonical resource roots are supplied;
        # C2=a DTO dependency reaches the second root again; C3=input order is
        # reversed. Effects: E1=resource transport sources remain excluded from
        # the DTO closure; E2=both orders produce identical evidence. Decision
        # table: C1+C2 with either C3 value => E1+E2; a path's first traversal
        # must never change its authoritative resource-root classification.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = root / "anthropic/resources/beta/widgets.py"
            second = root / "anthropic/resources/skills/skills.py"
            dto = root / "anthropic/types/beta/widget.py"
            first.parent.mkdir(parents=True)
            second.parent.mkdir(parents=True)
            dto.parent.mkdir(parents=True)
            first.write_text("from ...types.beta.widget import Widget\n", encoding="utf-8")
            second.write_text("class Skills: pass\n", encoding="utf-8")
            dto.write_text(
                "from ...resources.skills.skills import Skills\nclass Widget: pass\n",
                encoding="utf-8",
            )

            forward = oracle.managed_type_source_evidence(root, [first, second])
            reverse = oracle.managed_type_source_evidence(root, [second, first])

        self.assertEqual(forward, reverse)
        self.assertEqual(
            [item["path"] for item in forward],
            ["anthropic/types/beta/widget.py"],
        )

    def test_managed_type_closure_rejects_unresolved_or_wildcard_reexports(self) -> None:
        # Fail-closed partitions: a resource-level imported DTO must resolve to
        # one local definition or exact re-export. A wildcard or missing symbol
        # cannot produce a reviewable dependency edge and therefore cannot be
        # represented by a stable compatibility fingerprint.
        for initializer, failure in (
            ("from .anything import *\n", "wildcard export"),
            ("KNOWN = 1\n", "no auditable source"),
        ):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                resource = root / "anthropic/resources/beta/widgets.py"
                types = root / "anthropic/types/beta"
                resource.parent.mkdir(parents=True)
                types.mkdir(parents=True)
                resource.write_text(
                    "from ...types.beta import MissingThing\n",
                    encoding="utf-8",
                )
                (types / "__init__.py").write_text(initializer, encoding="utf-8")
                (types / "anything.py").write_text("pass\n", encoding="utf-8")
                with self.assertRaisesRegex(AssertionError, failure):
                    oracle.managed_type_source_evidence(root, [resource])

    def test_route_expression_rejects_unresolved_dynamic_values(self) -> None:
        # Negative partition: a route assembled outside a literal/path_template
        # cannot be fingerprinted from source and is rejected. Accepting it
        # would let an SDK route drift while the oracle remained unchanged.
        expression = ast.Name(id="dynamic_route")
        with self.assertRaisesRegex(AssertionError, "unsupported Python SDK route"):
            oracle.expression_text(expression)

    def test_wheel_extraction_rejects_parent_traversal(self) -> None:
        # Supply-chain partition: even a digest-qualified wheel cannot write
        # outside its disposable extraction root. A parent traversal member is
        # rejected before any SDK source is parsed or imported.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            wheel = root / "malicious.whl"
            with zipfile.ZipFile(wheel, "w") as archive:
                archive.writestr("../escape.py", "raise SystemExit")
            with self.assertRaisesRegex(AssertionError, "unsafe wheel member"):
                oracle.extract_wheel(wheel, root / "extract")
            self.assertFalse((root / "escape.py").exists())


if __name__ == "__main__":
    unittest.main()
