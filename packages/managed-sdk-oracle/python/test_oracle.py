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
