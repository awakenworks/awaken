"""Reviewed request preparation and stream-finalization change points."""
from __future__ import annotations

import base64
import datetime
import importlib
import io
import json
import typing
from pathlib import Path
from unittest.mock import patch
from urllib.parse import parse_qs


class OneShot:
    def __init__(self, values: list[object]) -> None:
        self.values = iter(values)
        self.iterations = 0

    def __iter__(self) -> OneShot:
        self.iterations += 1
        assert self.iterations == 1, "one-shot input must not be traversed on retry/page"
        return self

    def __next__(self) -> object:
        return next(self.values)


class ReadOnce(io.BytesIO):
    reads = 0

    def read(self, *args: object) -> bytes:
        self.reads += 1
        assert self.reads == 1, "file input must not be read again on retry"
        return super().read(*args)


async def exercise_request_preparation(anthropic, transport, call, *, asynchronous: bool) -> None:
    # Cause/effect graph: C1 POST body has one-shot iterators, dates, shared
    # acyclic containers, Omit/NotGiven, and a base64 file; C2 retry is required;
    # C3 GET has an unreadable extra body and paginated one-shot query; C4 a
    # container is cyclic; C5 Shared permission declarations exist.
    # R1 C1+C2 => identical method/URL/body/key twice; one traversal/read; caller
    # containers unchanged. R2 C3 => zero body reads; query preserved across
    # retry AND next page; top-level Omit removes the inherited default.
    # R3 C4 => ValueError before any HTTP. R4 C5 => both event annotations use
    # the exact shared allow/ask/deny alias. All R1-R4 run in sync AND async
    # modes via the existing transport fixture, not a second client factory.
    attempts = []
    values = OneShot([{"value": 1, "omit": anthropic.omit, "absent": anthropic.NOT_GIVEN}])
    file = ReadOnce(b"prepared-file-bytes")
    date = datetime.date(2026, 9, 22)
    shared = {"stable": [date]}
    body = {"prepared": {"values": values, "date": date, "a": shared, "b": shared},
            "source": {"type": "base64", "data": file}}
    query = OneShot(["x", "y"])

    def retry_once(request):
        attempts.append(request)
        return transport.Response(500 if len(attempts) == 1 else 200, request=request,
                                  headers={"retry-after-ms": "0"}, json={})

    await call(anthropic, transport, retry_once, asynchronous=asynchronous, max_retries=1,
               raw=True, extra_headers={"idempotency-key": "prepared-command"},
               call_arguments={"extra_body": body, "extra_query": {"tag": query, "drop": anthropic.omit}},
               client_options={"default_query": {"drop": "must-disappear"}})
    assert len(attempts) == 2, "R1 exact retry bound"
    first, second = attempts
    assert (first.method, first.url, first.content) == (second.method, second.url, second.content), "R1 byte identity"
    assert first.headers["idempotency-key"] == second.headers["idempotency-key"] == "prepared-command"
    assert json.loads(first.content) == {
        "agent": "fixture", "environment_id": "fixture",
        "prepared": {"values": [{"value": 1}], "date": "2026-09-22",
                     "a": {"stable": ["2026-09-22"]}, "b": {"stable": ["2026-09-22"]}},
        "source": {"type": "base64", "data": base64.b64encode(b"prepared-file-bytes").decode()},
    }, "R1 exact prepared values"
    assert parse_qs(first.url.query.decode())["tag[]"] == ["x", "y"]
    assert "drop" not in parse_qs(first.url.query.decode()), "R1 inherited query removal"
    assert values.iterations == query.iterations == file.reads == 1
    assert shared == {"stable": [date]} and body["prepared"]["a"] is body["prepared"]["b"], "R1 caller frame"
    assert body["source"]["data"] is file and body["prepared"]["values"] is values

    class Unreadable:
        def __iter__(self):
            raise AssertionError("GET extra_body was read")

    requests = []
    page_query = OneShot(["first", "second"])

    def pages(request):
        requests.append(request)
        assert request.content == b"", "R2 GET sends no body"
        if len(requests) == 1:
            return transport.Response(500, request=request, json={}, headers={"retry-after-ms": "0"})
        last = len(requests) == 3
        return transport.Response(200, request=request, json={
            "data": [{"id": "sesn_second" if last else "sesn_first"}],
            "next_page": None if last else "opaque-next-page",
        })

    rows = await call(anthropic, transport, pages, asynchronous=asynchronous, max_retries=1,
                      operation="list", collect=True, call_arguments={
                          "extra_body": {"unreadable": Unreadable()},
                          "extra_query": {"tag": page_query, "date": date, "path": Path("/not-read")},
                      })
    assert [row.id for row in rows] == ["sesn_first", "sesn_second"], "R2 both pages"
    assert len(requests) == 3 and page_query.iterations == 1, "R2 once across retry/page"
    for request in requests:
        parsed = parse_qs(request.url.query.decode())
        assert parsed["tag[]"] == ["first", "second"] and parsed["date"] == ["2026-09-22"]
        assert parsed["path"] == ["/not-read"], "R2 query path has no file I/O"
    assert requests[0].url == requests[1].url
    assert parse_qs(requests[2].url.query.decode())["page"] == ["opaque-next-page"]

    cyclic = {}
    cyclic["self"] = cyclic
    attempts.clear()
    try:
        await call(anthropic, transport, retry_once, asynchronous=asynchronous, max_retries=2,
                   call_arguments={"extra_body": cyclic})
    except ValueError as error:
        assert "Circular reference" in str(error), "R3 exact pre-HTTP rejection"
    else:
        raise AssertionError("R3 cyclic body was admitted")
    assert attempts == [], "R3 no request/retry"

    events = importlib.import_module("anthropic.types.beta.sessions")
    compat = importlib.import_module("anthropic._compat")
    permission = events.BetaManagedAgentsAgentEvaluatedPermission
    assert set(typing.get_args(permission)) == {"allow", "ask", "deny"}, "R4 closed vocabulary"
    for event in (events.BetaManagedAgentsAgentToolUseEvent, events.BetaManagedAgentsAgentMCPToolUseEvent):
        field = compat.get_model_fields(event)["evaluated_permission"]
        assert field.annotation == typing.Optional[permission], "R4 shared alias"

    # Stream causes: S1 consumer closes a partly read generator; S2 interpreter
    # finalization is false/true. S1+!S2 => close actual response once; S1+S2 =>
    # mark it closed without the shutdown-unsafe I/O. Both modes consume a real
    # SDK stream. This deterministic branch test is not a CPython 3.13 exit proof.
    for finalizing in (False, True):
        closes = []

        class ByteStream(transport.SyncByteStream, transport.AsyncByteStream):
            def __iter__(self):
                yield b'event: message\ndata: {"type":"probe","value":1}\n\n'

            async def __aiter__(self):
                yield b'event: message\ndata: {"type":"probe","value":1}\n\n'

            def close(self):
                closes.append("sync")

            async def aclose(self):
                closes.append("async")

        client_type = anthropic.AsyncAnthropic if asynchronous else anthropic.Anthropic
        client = client_type(api_key="stream-preparation-fixture")  # awaken-allow: secret
        response = transport.Response(200, stream=ByteStream(), request=transport.Request("GET", "https://fixture.invalid"))
        stream_type = anthropic.AsyncStream if asynchronous else anthropic.Stream
        stream = stream_type(cast_to=dict[str, object], response=response, client=client)
        try:
            if asynchronous:
                assert await stream.__anext__() == {"type": "probe", "value": 1}
            else:
                assert next(stream) == {"type": "probe", "value": 1}
            with patch("anthropic._streaming.sys.is_finalizing", return_value=finalizing):
                if asynchronous:
                    await stream._iterator.aclose()
                else:
                    stream._iterator.close()
            assert response.is_closed
            assert closes == ([] if finalizing else ["async" if asynchronous else "sync"]), "S1/S2 exact close effect"
        finally:
            if asynchronous:
                await client.close()
            else:
                client.close()
