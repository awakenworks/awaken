from __future__ import annotations

import asyncio
import collections.abc
import datetime
import enum
import importlib
import io
import inspect
import itertools
import os
import re
import types
import typing
from pathlib import Path
from typing import Any
from urllib.parse import parse_qsl, unquote

from managed_python_sdk_wire import semantic_request


SKILL = b"---\nname: python-fixture\ndescription: request fixture\n---\nFixture."
TRANSPORT_PARAMETERS = frozenset(
    {"betas", "extra_headers", "extra_query", "extra_body", "timeout"}
)
UPLOAD_MARKER = "__managed_python_upload_kind__"
MANAGED_SDK_ANCHOR_VERSIONS = frozenset({
    "0.92.0",
    "0.100.0",
    "0.109.0",
    "0.115.0",
    "0.116.0",
    "0.117.1",
    "0.118.0",
    "0.121.0",
    "0.124.0",
    "0.125.0",
    "1.0.0",
    "1.1.0",
    "1.2.0",
})
WORKSPACE_RESPONSE_CONTEXT_VERSIONS = frozenset({
    "0.124.0",
    "0.125.0",
    "1.0.0",
    "1.1.0",
    "1.2.0",
})
CREDENTIAL_PROVIDER_VERSIONS = MANAGED_SDK_ANCHOR_VERSIONS - {"0.92.0"}
ENVIRONMENT_HELPER_VERSIONS = MANAGED_SDK_ANCHOR_VERSIONS - {"0.92.0", "0.100.0"}
# The 0.109 deployment change point introduced both the environment helpers
# and middleware. Keep one closed version set so those coupled surfaces cannot
# silently acquire different historical boundaries.
MIDDLEWARE_VERSIONS = ENVIRONMENT_HELPER_VERSIONS


def projects_workspace_response_context(version: str) -> bool:
    """Return the reviewed official-wheel response projection boundary."""
    if version not in MANAGED_SDK_ANCHOR_VERSIONS:
        raise AssertionError(f"unreviewed Python Managed SDK version: {version}")
    return version in WORKSPACE_RESPONSE_CONTEXT_VERSIONS


PATHLIKE_REJECTING_VERSIONS = frozenset(
    {
        "0.92.0",
        "0.100.0",
        "0.109.0",
        "0.115.0",
        "0.116.0",
        "0.117.1",
        "0.118.0",
        "0.121.0",
    }
)
UNICODE_WORKER_HEADER_REJECTING_VERSIONS = frozenset(
    {
        # `anthropic_worker_id` first appears at the 0.109 Environment Work
        # change point. Every selected wheel from that introduction through
        # 0.125 delegates the value to an ASCII-only httpx Header encoder; the
        # 1.0 httpx2 transport boundary accepts the same declaration-derived
        # non-ASCII witness. Keeping the complete closed set here makes a
        # silent transport change fail in either direction.
        "0.109.0",
        "0.115.0",
        "0.116.0",
        "0.117.1",
        "0.118.0",
        "0.121.0",
        "0.124.0",
        "0.125.0",
    }
)
MULTIPART_HEADER_NAME_REJECTING_VERSIONS = frozenset({"1.0.0", "1.1.0"})
HTTP_FIELD_NAME = re.compile(r"[!#$%&'*+\-.^_`|~0-9A-Za-z]+\Z")


def required_fixture(name: str) -> object:
    fixtures: dict[str, object] = {
        "agent": "fixture",
        "auth": {
            "type": "static_bearer",
            "token": "fixture-token",  # awaken-allow: secret
            "mcp_server_url": "https://mcp.invalid",
        },
        "authorization_token": "fixture-token",  # awaken-allow: secret
        "ca_certificate_pem": "-----BEGIN CERTIFICATE-----\nfixture\n-----END CERTIFICATE-----",
        "content": "fixture",
        "display_name": "Fixture",
        "display_title": "Fixture",
        "environment_id": "fixture",
        "events": [{"type": "user.message", "content": [{"type": "text", "text": "fixture"}]}],
        "file": ("fixture.txt", b"fixture", "text/plain"),
        "files": [("SKILL.md", SKILL, "text/markdown")],
        "initial_events": [
            {"type": "user.message", "content": [{"type": "text", "text": "fixture"}]}
        ],
        "inputs": [{"type": "memory_store", "memory_store_id": "fixture"}],
        "metadata": {"fixture": "value"},
        "model": "claude-opus-5",
        "name": "Fixture",
        "path": "/fixture.md",
        "type": "file",
        "version": "fixture",
    }
    if name.endswith("_id"):
        return "fixture"
    try:
        return fixtures[name]
    except KeyError as error:
        raise AssertionError(f"no reviewed Python request fixture for required parameter {name}") from error


def resource_method(client: object, operation_id: str) -> object:
    parts = operation_id.split(".")
    resource = client
    for part in parts[:-1]:
        resource = getattr(resource, part)
    return getattr(getattr(resource, "with_raw_response"), parts[-1])


def required_arguments(method: object) -> tuple[list[object], dict[str, object]]:
    positional = []
    keyword = {}
    for parameter in inspect.signature(method).parameters.values():
        if parameter.default is not inspect.Parameter.empty:
            continue
        value = required_fixture(parameter.name)
        if parameter.kind in (
            inspect.Parameter.POSITIONAL_ONLY,
            inspect.Parameter.POSITIONAL_OR_KEYWORD,
        ):
            positional.append(value)
        elif parameter.kind == inspect.Parameter.KEYWORD_ONLY:
            keyword[parameter.name] = value
        else:
            raise AssertionError(
                f"unsupported required parameter kind {parameter.kind} for {parameter.name}"
            )
    return positional, keyword


def _identity(value: object) -> object:
    if isinstance(value, dict):
        return ("dict", tuple(sorted((name, _identity(nested)) for name, nested in value.items())))
    if isinstance(value, (list, tuple)):
        return (type(value).__name__, tuple(_identity(item) for item in value))
    if isinstance(value, bytes):
        return ("bytes", value)
    if isinstance(value, (datetime.date, datetime.datetime, Path)):
        return (type(value).__name__, str(value))
    if isinstance(value, enum.Enum):
        return (type(value).__qualname__, value.value)
    return (type(value).__name__, repr(value))


def _distinct(values: list[object]) -> list[object]:
    return list(dict((_identity(value), value) for value in values).values())


def _without_sdk_sentinels(types_: tuple[object, ...]) -> tuple[object, ...]:
    return tuple(
        candidate
        for candidate in types_
        if not (
            inspect.isclass(candidate)
            and candidate.__module__.startswith("anthropic")
            and candidate.__name__ in {"Omit", "NotGiven"}
        )
    )


def _unwrap(annotation: object) -> object:
    while typing.get_origin(annotation) in {
        typing.Annotated,
        typing.Required,
        typing.NotRequired,
    }:
        annotation = typing.get_args(annotation)[0]
    return annotation


def _baseline_type_value(annotation: object, stack: tuple[object, ...] = ()) -> object:
    values = _type_witnesses(annotation, stack)
    assert values, f"request type has no constructible branch: {annotation!r}"
    def preferred(value: object) -> bool:
        if value is None:
            return False
        if isinstance(value, str):
            return bool(value)
        if isinstance(value, bool):
            return value
        if isinstance(value, (int, float)):
            return value > 0
        if isinstance(value, (dict, list, tuple)):
            return bool(value)
        return True

    return next(
        (value for value in values if preferred(value)),
        next((value for value in values if value is not None), values[0]),
    )


def _typed_dict_witnesses(
    annotation: object,
    stack: tuple[object, ...],
) -> list[object]:
    assert annotation not in stack, f"recursive request TypedDict is not finite: {annotation!r}"
    hints = typing.get_type_hints(annotation, include_extras=True)
    required = set(annotation.__required_keys__)
    for name, field in hints.items():
        origin = typing.get_origin(field)
        if origin is typing.Required:
            required.add(name)
        elif annotation.__total__ and origin is not typing.NotRequired:
            required.add(name)
    nested_stack = (*stack, annotation)
    baseline = {
        name: _baseline_type_value(field, nested_stack)
        for name, field in hints.items()
        if name in required
    }
    generated: list[object] = [baseline]
    for name, field in hints.items():
        for value in _type_witnesses(field, nested_stack):
            generated.append({**baseline, name: value})
    return _distinct(generated)


def _tuple_witnesses(args: tuple[object, ...], stack: tuple[object, ...]) -> list[object]:
    if len(args) == 2 and args[1] is Ellipsis:
        return [(), *[(value,) for value in _type_witnesses(args[0], stack)]]
    baseline = tuple(_baseline_type_value(item, stack) for item in args)
    generated: list[object] = [baseline]
    for index, item in enumerate(args):
        for value in _type_witnesses(item, stack):
            candidate = list(baseline)
            candidate[index] = value
            generated.append(tuple(candidate))
    return _distinct(generated)


def _type_witnesses(annotation: object, stack: tuple[object, ...] = ()) -> list[object]:
    """Finite one-factor witnesses for one installed SDK request annotation.

    The recursion mirrors the declaration's algebra rather than operation names:
    every union arm, literal, nullable branch, TypedDict field omission/presence,
    empty/non-empty collection, mapping value and upload representation is
    executable. Unknown or recursive shapes fail closed when an SDK evolves.
    """

    annotation = _unwrap(annotation)
    if annotation in {Any, object, typing.Any}:
        return [{"managed_fixture": {"enabled": True, "nullable": None}}]
    if annotation in {None, type(None)}:
        return [None]
    if annotation is str:
        return ["", "managed fixture /?% ü"]
    if annotation is bool:
        return [True, False]
    if annotation is int:
        return [-1, 0, 1]
    if annotation is float:
        return [-1.5, 0.0, 1.5]
    if annotation is bytes:
        return [{UPLOAD_MARKER: "bytes"}]
    if annotation in {datetime.datetime, datetime.date}:
        return [datetime.datetime(2026, 1, 2, 3, 4, 5, tzinfo=datetime.timezone.utc)]
    if annotation is os.PathLike:
        return [{UPLOAD_MARKER: "path"}]
    if isinstance(annotation, typing.TypeVar):
        if annotation.__constraints__:
            return _distinct(
                [
                    value
                    for constraint in annotation.__constraints__
                    for value in _type_witnesses(constraint, stack)
                ]
            )
        if annotation.__bound__ is not None:
            return _type_witnesses(annotation.__bound__, stack)
        raise AssertionError(f"unbounded request TypeVar: {annotation!r}")
    if hasattr(annotation, "__supertype__"):
        return _type_witnesses(annotation.__supertype__, stack)
    if typing.is_typeddict(annotation) or (
        inspect.isclass(annotation)
        and issubclass(annotation, dict)
        and hasattr(annotation, "__required_keys__")
        and hasattr(annotation, "__optional_keys__")
    ):
        return _typed_dict_witnesses(annotation, stack)
    if inspect.isclass(annotation) and issubclass(annotation, enum.Enum):
        return list(annotation)

    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    if origin is typing.Literal:
        return list(args)
    if origin in {typing.Union, types.UnionType}:
        return _distinct(
            [
                value
                for candidate in _without_sdk_sentinels(args)
                for value in _type_witnesses(candidate, stack)
            ]
        )
    if origin in {tuple, typing.Tuple}:
        return _tuple_witnesses(args, stack)
    if origin in {typing.IO, typing.BinaryIO}:
        return [{UPLOAD_MARKER: "io"}]
    if origin is os.PathLike:
        return [{UPLOAD_MARKER: "path"}]
    if origin is not None and inspect.isclass(origin):
        if issubclass(origin, collections.abc.Mapping):
            key_type, value_type = args or (str, object)
            key = _baseline_type_value(key_type, stack)
            assert isinstance(key, str), f"request map key must be a string: {annotation!r}"
            return [
                {},
                *[{key: value} for value in _type_witnesses(value_type, stack)],
            ]
        if issubclass(origin, collections.abc.Iterable):
            assert args, f"unparameterized request collection: {annotation!r}"
            return [
                [],
                *[[value] for value in _type_witnesses(args[0], stack)],
            ]
    raise AssertionError(f"unsupported installed SDK request type: {annotation!r}")


def _materialize(value: object) -> object:
    if isinstance(value, dict) and set(value) == {UPLOAD_MARKER}:
        kind = value[UPLOAD_MARKER]
        if kind == "bytes":
            return b"managed-python-upload"
        if kind == "io":
            return io.BytesIO(b"managed-python-upload")
        if kind == "path":
            # PathLike itself has one change-point probe below. The declaration
            # witness uses a fresh equivalent stream so the same central SDK
            # upload defect is not multiplied across every FileTypes nesting.
            return io.BytesIO(Path(__file__).read_bytes())
        raise AssertionError(f"unknown upload witness {kind!r}")
    if isinstance(value, list):
        return [_materialize(item) for item in value]
    if isinstance(value, tuple):
        return tuple(_materialize(item) for item in value)
    if isinstance(value, dict):
        return {name: _materialize(nested) for name, nested in value.items()}
    return value


def _declared_invocations(method: object) -> list[tuple[object, list[object], dict[str, object]]]:
    signature = inspect.signature(method)
    hints = typing.get_type_hints(method, include_extras=True)
    parameters = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.name not in TRANSPORT_PARAMETERS
    ]
    assert all(parameter.name in hints for parameter in parameters), (
        "every public request parameter has a runtime-resolvable annotation"
    )
    baseline_values = {
        parameter.name: _baseline_type_value(hints[parameter.name])
        for parameter in parameters
        if parameter.default is inspect.Parameter.empty
    }
    cases: list[tuple[str, dict[str, object]]] = [("required-only", baseline_values)]
    for parameter in parameters:
        for value in _type_witnesses(hints[parameter.name]):
            cases.append((parameter.name, {**baseline_values, parameter.name: value}))

    for parameter in parameters:
        if parameter.default is not inspect.Parameter.empty:
            assert parameter.name not in baseline_values, (
                f"{parameter.name}: optional omission witness"
            )
        expected = {_identity(value) for value in _type_witnesses(hints[parameter.name])}
        observed = {
            _identity(supplied[parameter.name])
            for cause, supplied in cases
            if cause == parameter.name
        }
        assert observed == expected, f"{parameter.name}: declaration branches changed"

    invocations = []
    seen = set()
    for cause, supplied in cases:
        identity = (cause, _identity(supplied))
        if identity in seen:
            continue
        seen.add(identity)
        positional = []
        keyword = {}
        for parameter in parameters:
            if parameter.name not in supplied:
                continue
            value = supplied[parameter.name]
            if parameter.kind == inspect.Parameter.POSITIONAL_ONLY:
                positional.append(value)
            elif parameter.kind in {
                inspect.Parameter.POSITIONAL_OR_KEYWORD,
                inspect.Parameter.KEYWORD_ONLY,
            }:
                keyword[parameter.name] = value
            else:
                raise AssertionError(
                    f"unsupported request parameter kind {parameter.kind}: {parameter.name}"
                )
        invocations.append((identity, positional, keyword))

    baseline_identity = ("required-only", _identity(baseline_values))
    assert any(identity == baseline_identity for identity, _, _ in invocations)
    return invocations


def _assert_path_template(operation: dict[str, Any], actual_path: str) -> None:
    expected = operation["path"].split("/")
    actual = actual_path.split("/")
    assert len(actual) == len(expected), operation["id"]
    assert all(want == "{}" or want == unquote(got) for want, got in zip(expected, actual)), (
        operation["id"]
    )


def _assert_declared_coordinate(operation: dict[str, Any], request: object) -> None:
    assert request.method == operation["method"], operation["id"]
    _assert_path_template(
        operation,
        request.url.raw_path.split(b"?", 1)[0].decode(),
    )
    fixed_query = operation.get("transport_query", "")
    if fixed_query:
        for pair in parse_qsl(fixed_query, keep_blank_values=True):
            assert pair in parse_qsl(request.url.query.decode(), keep_blank_values=True), (
                operation["id"]
            )
    actual_betas = set(filter(None, request.headers.get("anthropic-beta", "").split(",")))
    assert set(operation["betas"]).issubset(actual_betas), operation["id"]


def _empty_path_field(
    method: object,
    operation: dict[str, Any],
    positional: list[object],
    keyword: dict[str, object],
) -> str | None:
    signature = inspect.signature(method)
    public_parameters = [
        parameter
        for parameter in signature.parameters.values()
        if parameter.name not in TRANSPORT_PARAMETERS
    ]
    path_count = operation["path"].count("{}")
    assert path_count <= len(public_parameters), operation["id"]
    bound = signature.bind_partial(*positional, **keyword)
    empty = [
        parameter.name
        for parameter in public_parameters[:path_count]
        if bound.arguments.get(parameter.name) == ""
    ]
    assert len(empty) <= 1, f"{operation['id']}: one-factor empty path witness"
    return empty[0] if empty else None


def _assert_empty_path_rejection(error: BaseException, field: str, operation_id: str) -> None:
    assert type(error) is ValueError, operation_id
    assert str(error) == (
        f"Expected a non-empty value for `{field}` but received ''"
    ), operation_id


def _unicode_worker_header_field(
    version: str,
    operation_id: str,
    keyword: dict[str, object],
) -> str | None:
    field = "anthropic_worker_id"
    value = keyword.get(field)
    if (
        version in UNICODE_WORKER_HEADER_REJECTING_VERSIONS
        and operation_id == "beta.environments.work.poll"
        and isinstance(value, str)
        and not value.isascii()
    ):
        return field
    return None


def _assert_unicode_header_rejection(
    error: BaseException,
    field: str,
    operation_id: str,
) -> None:
    assert isinstance(error, UnicodeEncodeError), operation_id
    assert error.encoding == "ascii" and error.start < error.end, operation_id
    assert not str(error.object).isascii(), operation_id
    assert field == "anthropic_worker_id", operation_id


def _contains_invalid_multipart_header(value: object) -> bool:
    if isinstance(value, tuple):
        if (
            len(value) == 4
            and isinstance(value[3], collections.abc.Mapping)
            and any(
                not isinstance(name, str) or HTTP_FIELD_NAME.fullmatch(name) is None
                for name in value[3]
            )
        ):
            return True
        return any(_contains_invalid_multipart_header(item) for item in value)
    if isinstance(value, list):
        return any(_contains_invalid_multipart_header(item) for item in value)
    return False


def _invalid_multipart_header_field(
    version: str,
    keyword: dict[str, object],
) -> str | None:
    # 1.0 and 1.1 expose the four-part FileTypes declaration unchanged but
    # httpx2 rejects a non-token custom multipart header name before transport.
    # 0.x httpx accepts the witness; 1.2 normalizes the upload representation.
    if version not in MULTIPART_HEADER_NAME_REJECTING_VERSIONS:
        return None
    matches = [
        name
        for name, value in keyword.items()
        if _contains_invalid_multipart_header(value)
    ]
    assert len(matches) <= 1, "one-factor multipart-header witness"
    return matches[0] if matches else None


def _assert_multipart_header_rejection(
    error: BaseException,
    field: str,
    operation_id: str,
) -> None:
    assert type(error) is ValueError, operation_id
    assert str(error) == "Invalid multipart header name.", operation_id
    assert field in {"file", "files"}, operation_id


def exercise_declared_request_witnesses(
    anthropic_module: Any,
    transport_module: Any,
    operations: list[dict[str, Any]],
) -> int:
    # Historical declaration causal graph:
    # exact wheel source/hash -> get_type_hints -> finite one-factor witnesses
    # -> official sync serializer -> semantic Request authority
    # -> official async serializer -> exact semantic equality.
    # Every operation has a required-only baseline; every optional/null/union/
    # literal/nested/collection/map/upload branch changes one cause at a time.
    # Unknown annotation, missing async branch, serializer rejection, duplicate
    # request, or method/path/query/header/body drift fails closed.
    expected: list[tuple[str, object, dict[str, Any]]] = []
    sync_requests = []

    def respond(request: object) -> object:
        sync_requests.append(request)
        return transport_module.Response(200, json={})

    with anthropic_module.Anthropic(
        api_key="declared-request-sync",  # awaken-allow: secret
        http_client=transport_module.Client(
            transport=transport_module.MockTransport(respond)
        ),
        max_retries=0,
    ) as client:
        for operation in operations:
            method = resource_method(client, operation["id"])
            for identity, positional, keyword in _declared_invocations(method):
                before = len(sync_requests)
                field = _empty_path_field(method, operation, positional, keyword)
                header_field = _unicode_worker_header_field(
                    anthropic_module.__version__,
                    operation["id"],
                    keyword,
                )
                multipart_field = _invalid_multipart_header_field(
                    anthropic_module.__version__,
                    keyword,
                )
                try:
                    response = method(
                        *[_materialize(value) for value in positional],
                        **{name: _materialize(value) for name, value in keyword.items()},
                    )
                except (ValueError, UnicodeEncodeError) as error:
                    assert sum(
                        candidate is not None
                        for candidate in (field, header_field, multipart_field)
                    ) == 1, (
                        f"{operation['id']}: unreviewed sync serializer rejection "
                        f"for {identity[0]}: {error!r}"
                    )
                    if field is not None:
                        _assert_empty_path_rejection(error, field, operation["id"])
                    elif header_field is not None:
                        _assert_unicode_header_rejection(
                            error,
                            header_field,
                            operation["id"],
                        )
                    else:
                        assert multipart_field is not None
                        _assert_multipart_header_rejection(
                            error,
                            multipart_field,
                            operation["id"],
                        )
                    assert len(sync_requests) == before
                    expected.append((operation["id"], identity, {
                        "kind": (
                            "empty-path-rejection"
                            if field is not None
                            else (
                                "unicode-header-rejection"
                                if header_field is not None
                                else "multipart-header-rejection"
                            )
                        ),
                        "field": field or header_field or multipart_field,
                    }))
                    continue
                assert field is None and header_field is None and multipart_field is None, (
                    f"{operation['id']}: expected sync serializer rejection vanished"
                )
                assert response.status_code == 200
                assert len(sync_requests) == before + 1
                request = sync_requests[-1]
                _assert_declared_coordinate(operation, request)
                expected.append((operation["id"], identity, {
                    "kind": "request",
                    "semantic": semantic_request(request),
                }))

    async def exercise_async() -> None:
        async_requests = []

        async def respond(request: object) -> object:
            async_requests.append(request)
            return transport_module.Response(200, json={})

        observed: list[tuple[str, object, dict[str, Any]]] = []
        async with anthropic_module.AsyncAnthropic(
            api_key="declared-request-async",  # awaken-allow: secret
            http_client=transport_module.AsyncClient(
                transport=transport_module.MockTransport(respond)
            ),
            max_retries=0,
        ) as client:
            for operation in operations:
                method = resource_method(client, operation["id"])
                for identity, positional, keyword in _declared_invocations(method):
                    before = len(async_requests)
                    field = _empty_path_field(method, operation, positional, keyword)
                    header_field = _unicode_worker_header_field(
                        anthropic_module.__version__,
                        operation["id"],
                        keyword,
                    )
                    multipart_field = _invalid_multipart_header_field(
                        anthropic_module.__version__,
                        keyword,
                    )
                    try:
                        response = await method(
                            *[_materialize(value) for value in positional],
                            **{name: _materialize(value) for name, value in keyword.items()},
                        )
                    except (ValueError, UnicodeEncodeError) as error:
                        assert sum(
                            candidate is not None
                            for candidate in (field, header_field, multipart_field)
                        ) == 1, (
                            f"{operation['id']}: unreviewed async serializer rejection "
                            f"for {identity[0]}: {error!r}"
                        )
                        if field is not None:
                            _assert_empty_path_rejection(error, field, operation["id"])
                        elif header_field is not None:
                            _assert_unicode_header_rejection(
                                error,
                                header_field,
                                operation["id"],
                            )
                        else:
                            assert multipart_field is not None
                            _assert_multipart_header_rejection(
                                error,
                                multipart_field,
                                operation["id"],
                            )
                        assert len(async_requests) == before
                        observed.append((operation["id"], identity, {
                            "kind": (
                                "empty-path-rejection"
                                if field is not None
                                else (
                                    "unicode-header-rejection"
                                    if header_field is not None
                                    else "multipart-header-rejection"
                                )
                            ),
                            "field": field or header_field or multipart_field,
                        }))
                        continue
                    assert (
                        field is None
                        and header_field is None
                        and multipart_field is None
                    ), (
                        f"{operation['id']}: expected async serializer rejection vanished"
                    )
                    assert response.status_code == 200
                    assert len(async_requests) == before + 1
                    request = async_requests[-1]
                    _assert_declared_coordinate(operation, request)
                    observed.append((operation["id"], identity, {
                        "kind": "request",
                        "semantic": semantic_request(request),
                    }))
        if observed != expected:
            difference = next(
                (
                    (index, sync, asynchronous)
                    for index, (sync, asynchronous) in enumerate(
                        itertools.zip_longest(expected, observed)
                    )
                    if sync != asynchronous
                ),
                None,
            )
            raise AssertionError(
                f"historical sync/async semantic request drift: {difference!r}"
            )

    asyncio.run(exercise_async())
    assert len({operation_id for operation_id, _, _ in expected}) == len(operations)
    assert len(expected) > len(operations)
    path_rejections = sum(
        outcome["kind"] == "empty-path-rejection"
        for _, _, outcome in expected
    )
    expected_path_rejections = sum(operation["path"].count("{}") for operation in operations)
    assert path_rejections == expected_path_rejections, (
        f"{anthropic_module.__version__}: exact empty-path rejection closure"
    )
    unicode_header_rejections = sum(
        outcome["kind"] == "unicode-header-rejection"
        for _, _, outcome in expected
    )
    assert unicode_header_rejections == (
        1
        if anthropic_module.__version__ in UNICODE_WORKER_HEADER_REJECTING_VERSIONS
        else 0
    ), f"{anthropic_module.__version__}: exact Unicode worker-header change point"
    multipart_header_rejections = sum(
        outcome["kind"] == "multipart-header-rejection"
        for _, _, outcome in expected
    )
    assert (multipart_header_rejections > 0) == (
        anthropic_module.__version__ in MULTIPART_HEADER_NAME_REJECTING_VERSIONS
    ), f"{anthropic_module.__version__}: exact multipart-header change point"
    return len(expected)


def exercise_pathlike_upload_change_point(
    anthropic_module: Any,
    transport_module: Any,
    operation_ids: set[str],
) -> str:
    # Upstream declaration/runtime decision table:
    # C1 every fixed wheel declares PathLike in FileTypes; C2 versions through
    # 0.121 hand tuple-contained PosixPath to old httpx multipart and fail before
    # transport; C3 the 0.124 Files/Skills GA transform and later wheels serialize
    # it. Effects: C2 must preserve the exact APIConnectionError
    # <- AttributeError cause and zero requests; C3 must emit exactly one request.
    # A version entering/leaving either set fails and requires an explicit review.
    operation_id = next(
        (
            candidate
            for candidate in ("files.upload", "beta.files.upload")
            if candidate in operation_ids
        ),
        None,
    )
    assert operation_id is not None, "PathLike probe requires one Files upload operation"
    version = anthropic_module.__version__
    path = Path(__file__)
    cases = [
        ("direct", path, False),
        ("tuple2", (None, path), version in PATHLIKE_REJECTING_VERSIONS),
        ("tuple3", (None, path, None), version in PATHLIKE_REJECTING_VERSIONS),
        ("tuple4", (None, path, None, {}), version in PATHLIKE_REJECTING_VERSIONS),
    ]

    def assert_old_httpx_rejection(error: BaseException) -> None:
        assert type(error).__name__ == "APIConnectionError"
        cause = error
        while cause.__cause__ is not None:
            cause = cause.__cause__
        assert isinstance(cause, AttributeError)
        assert "PosixPath" in str(cause) and "read" in str(cause)

    sync_requests = []

    def sync_respond(request: object) -> object:
        sync_requests.append(request)
        return transport_module.Response(200, json={})

    with anthropic_module.Anthropic(
        api_key="pathlike-sync",  # awaken-allow: secret
        http_client=transport_module.Client(
            transport=transport_module.MockTransport(sync_respond)
        ),
        max_retries=0,
    ) as client:
        method = resource_method(client, operation_id)
        for label, value, expected_rejection in cases:
            before = len(sync_requests)
            try:
                response = method(file=value)
            except BaseException as error:
                assert expected_rejection, (
                    f"{version}: unreviewed sync PathLike rejection for {label}"
                )
                assert_old_httpx_rejection(error)
                assert len(sync_requests) == before
            else:
                assert not expected_rejection, (
                    f"{version}: expected sync PathLike rejection vanished for {label}"
                )
                assert response.status_code == 200
                assert len(sync_requests) == before + 1

    async def exercise_async() -> None:
        async_requests = []

        async def async_respond(request: object) -> object:
            async_requests.append(request)
            return transport_module.Response(200, json={})

        async with anthropic_module.AsyncAnthropic(
            api_key="pathlike-async",  # awaken-allow: secret
            http_client=transport_module.AsyncClient(
                transport=transport_module.MockTransport(async_respond)
            ),
            max_retries=0,
        ) as client:
            method = resource_method(client, operation_id)
            for label, value, expected_rejection in cases:
                before = len(async_requests)
                try:
                    response = await method(file=value)
                except BaseException as error:
                    assert expected_rejection, (
                        f"{version}: unreviewed async PathLike rejection for {label}"
                    )
                    assert_old_httpx_rejection(error)
                    assert len(async_requests) == before
                else:
                    assert not expected_rejection, (
                        f"{version}: expected async PathLike rejection vanished for {label}"
                    )
                    assert response.status_code == 200
                    assert len(async_requests) == before + 1

    asyncio.run(exercise_async())
    return (
        "tuple-upstream-rejection"
        if version in PATHLIKE_REJECTING_VERSIONS
        else "all-serialized"
    )


def normalized_actual_path(path: str) -> str:
    return "/".join("{}" if segment == "fixture" else segment for segment in path.split("/"))


def assert_operation_request(operation: dict[str, Any], request: object) -> None:
    assert request.method == operation["method"], operation["id"]
    assert normalized_actual_path(request.url.path) == operation["path"], operation["id"]
    assert request.url.query.decode() == operation.get("transport_query", ""), operation["id"]
    actual_betas = sorted(filter(None, request.headers.get("anthropic-beta", "").split(",")))
    assert actual_betas == operation["betas"], operation["id"]


def canonical_error(status: int, error_type: str) -> dict[str, object]:
    return {
        "type": "error",
        "error": {"type": error_type, "message": f"fixture {status}"},
        "request_id": f"req_body_{status}",
    }


async def _call_session_create(
    anthropic_module: Any,
    transport_module: Any,
    handler: object,
    *,
    asynchronous: bool,
    max_retries: int,
    raw: bool = False,
    extra_headers: dict[str, str] | None = None,
    client_options: dict[str, object] | None = None,
) -> object:
    arguments = {"agent": "fixture", "environment_id": "fixture"}
    if extra_headers is not None:
        arguments["extra_headers"] = extra_headers
    if asynchronous:
        async def async_handler(request: object) -> object:
            return handler(request)

        options = {
            "api_key": "async-transport-contract",  # awaken-allow: secret
            "http_client": transport_module.AsyncClient(
                transport=transport_module.MockTransport(async_handler)
            ),
            "max_retries": max_retries,
        }
        options.update(client_options or {})
        async with anthropic_module.AsyncAnthropic(
            **options,
        ) as client:
            method = (
                client.beta.sessions.with_raw_response.create
                if raw
                else client.beta.sessions.create
            )
            return await method(**arguments)

    options = {
        "api_key": "transport-contract",  # awaken-allow: secret
        "http_client": transport_module.Client(
            transport=transport_module.MockTransport(handler)
        ),
        "max_retries": max_retries,
    }
    options.update(client_options or {})
    with anthropic_module.Anthropic(**options) as client:
        method = (
            client.beta.sessions.with_raw_response.create
            if raw
            else client.beta.sessions.create
        )
        return method(**arguments)


async def _exercise_error_and_retry_contract_for_mode(
    anthropic_module: Any,
    transport_module: Any,
    *,
    asynchronous: bool,
) -> None:
    # Transport decision table shared across the exact Python version matrix:
    # C1=one canonical Managed error envelope and request-id header; C2=status
    # is 400/401/403/404/409/413/422/429/500/529; C3=max_retries=0. Effects: E1=the
    # exact SDK exception subclass/status/type/request-id is observable; E2=one
    # request only. Retry decision table: C4=status/override selects retry or
    # rejection and C5=max_retries=2; E3=exactly three or one attempts. Mutation
    # relation: C6=one retry with an explicit idempotency key and body; E4=the
    # complete command identity is byte-stable. C8=ConnectError followed by a
    # valid response and max_retries=1; E5=one retry and a decoded response.
    # C9=ReadTimeout with max_retries=0; E6=the exact APITimeoutError after one
    # attempt, distinct from APIConnectionError. C10=ConnectError with
    # max_retries=0; E7=the exact APIConnectionError and cause after one
    # attempt. This owns Python transport
    # behavior only; Awaken's production error mapping is owned by deployed/Rust
    # operation cases. C7 projects the same decision graph through sync and
    # async API clients; the transport handler and all expectations remain
    # single-owned, so a mode-specific divergence cannot be normalized away.
    cases = (
        (400, "invalid_request_error", "BadRequestError"),
        (401, "authentication_error", "AuthenticationError"),
        (403, "permission_error", "PermissionDeniedError"),
        (404, "not_found_error", "NotFoundError"),
        (408, "request_timeout", "APIStatusError"),
        (409, "conflict_error", "ConflictError"),
        (413, "request_too_large", "RequestTooLargeError"),
        (422, "invalid_request_error", "UnprocessableEntityError"),
        (429, "rate_limit_error", "RateLimitError"),
        (500, "api_error", "InternalServerError"),
        (529, "overloaded_error", "OverloadedError"),
    )
    for status, error_type, class_name in cases:
        requests = []

        def reject(request: object) -> object:
            requests.append(request)
            return transport_module.Response(
                status,
                request=request,
                json=canonical_error(status, error_type),
                headers={
                    "request-id": f"req_header_{status}",
                    "anthropic-workspace-id": f"workspace_header_{status}",
                },
            )

        try:
            await _call_session_create(
                anthropic_module,
                transport_module,
                reject,
                asynchronous=asynchronous,
                max_retries=0,
            )
        except anthropic_module.APIStatusError as error:
            assert error.__class__.__name__ == class_name
            assert error.status_code == status
            assert error.body["error"]["type"] == error_type
            assert error.request_id == f"req_header_{status}"
            assert error.response.headers["anthropic-workspace-id"] == (
                f"workspace_header_{status}"
            )
            projects_workspace = projects_workspace_response_context(
                anthropic_module.__version__
            )
            assert hasattr(error, "workspace_id") is projects_workspace
            if projects_workspace:
                assert error.workspace_id == f"workspace_header_{status}"
        else:
            raise AssertionError(f"{status}: Python SDK accepted a Managed error")
        assert len(requests) == 1, f"{status}: client fault retried"

    retry_cases = (
        (400, False, None),
        (408, True, None),
        (409, True, None),
        (413, False, None),
        (422, False, None),
        (429, True, None),
        (500, True, None),
        (529, True, None),
        (400, True, "true"),
        (500, False, "false"),
    )
    for status, retries, override in retry_cases:
        attempts = []

        def decide(request: object) -> object:
            attempts.append(request)
            if retries and len(attempts) == 3:
                return transport_module.Response(200, request=request, json={})
            headers = {"retry-after-ms": "0"}
            if override is not None:
                headers["x-should-retry"] = override
            return transport_module.Response(
                status,
                request=request,
                json=canonical_error(status, "api_error"),
                headers=headers,
            )

        try:
            response = await _call_session_create(
                anthropic_module,
                transport_module,
                decide,
                asynchronous=asynchronous,
                max_retries=2,
                raw=True,
            )
        except anthropic_module.APIStatusError as error:
            assert not retries, f"{status}/{override}: retryable response was rejected"
            assert error.status_code == status
        else:
            assert retries, f"{status}/{override}: non-retryable response was accepted"
            assert response.status_code == 200
        assert len(attempts) == (3 if retries else 1), (
            f"{status}/{override}: exact retry bound"
        )

    attempts = []

    def transient(request: object) -> object:
        attempts.append(request)
        if len(attempts) == 1:
            return transport_module.Response(
                500,
                request=request,
                json=canonical_error(500, "api_error"),
                headers={"request-id": "req_retry_1", "retry-after-ms": "0"},
            )
        return transport_module.Response(200, request=request, json={})

    key = "python-managed-retry-identity"
    response = await _call_session_create(
        anthropic_module,
        transport_module,
        transient,
        asynchronous=asynchronous,
        max_retries=1,
        raw=True,
        extra_headers={"idempotency-key": key},
    )
    assert response.status_code == 200
    assert len(attempts) == 2
    first, second = attempts
    assert (first.method, first.url, first.content) == (second.method, second.url, second.content)
    assert first.headers["idempotency-key"] == second.headers["idempotency-key"] == key

    failed_connection_attempts = []

    def disconnect(request: object) -> object:
        failed_connection_attempts.append(request)
        raise transport_module.ConnectError(
            "connection closed",
            request=request,
        )

    try:
        await _call_session_create(
            anthropic_module,
            transport_module,
            disconnect,
            asynchronous=asynchronous,
            max_retries=0,
        )
    except anthropic_module.APIConnectionError as error:
        assert error.__class__ is anthropic_module.APIConnectionError
        assert isinstance(error.__cause__, transport_module.ConnectError)
    else:
        raise AssertionError("Python SDK accepted a connection failure")
    assert len(failed_connection_attempts) == 1

    connection_attempts = []

    def reconnect(request: object) -> object:
        connection_attempts.append(request)
        if len(connection_attempts) == 1:
            raise transport_module.ConnectError(
                "connection closed",
                request=request,
            )
        return transport_module.Response(200, request=request, json={})

    response = await _call_session_create(
        anthropic_module,
        transport_module,
        reconnect,
        asynchronous=asynchronous,
        max_retries=1,
        raw=True,
    )
    assert response.status_code == 200
    assert len(connection_attempts) == 2

    timeout_attempts = []

    def time_out(request: object) -> object:
        timeout_attempts.append(request)
        raise transport_module.ReadTimeout(
            "SDK deadline elapsed",
            request=request,
        )

    try:
        await _call_session_create(
            anthropic_module,
            transport_module,
            time_out,
            asynchronous=asynchronous,
            max_retries=0,
        )
    except anthropic_module.APITimeoutError as error:
        assert error.__class__ is anthropic_module.APITimeoutError
        assert isinstance(error.__cause__, transport_module.ReadTimeout)
    else:
        raise AssertionError("Python SDK accepted a transport timeout")
    assert len(timeout_attempts) == 1

    await _exercise_credential_provider_contract_for_mode(
        anthropic_module,
        transport_module,
        asynchronous=asynchronous,
    )
    await _exercise_middleware_contract_for_mode(
        anthropic_module,
        transport_module,
        asynchronous=asynchronous,
    )


async def _exercise_credential_provider_contract_for_mode(
    anthropic_module: Any,
    transport_module: Any,
    *,
    asynchronous: bool,
) -> None:
    version = anthropic_module.__version__
    client_type = (
        anthropic_module.AsyncAnthropic
        if asynchronous
        else anthropic_module.Anthropic
    )
    supports_credentials = "credentials" in inspect.signature(client_type).parameters
    assert supports_credentials is (version in CREDENTIAL_PROVIDER_VERSIONS), (
        f"{version}: unreviewed credential-provider change point"
    )
    if not supports_credentials:
        return

    # Authentication state machine shared by sync and async official clients:
    # unresolved provider -> cached stale Bearer -> 401 -> forced refresh ->
    # fresh Bearer -> 200. Static api_key/auth_token instead short-circuit the
    # provider. This is deliberately part of the existing transport contract,
    # not a second credential inventory.
    credential_module = importlib.import_module("anthropic.lib.credentials")
    provider_calls = []
    requests = []

    def credentials(*, force_refresh: bool = False) -> object:
        provider_calls.append(force_refresh)
        token = "provider-stale" if len(provider_calls) == 1 else "provider-fresh"  # awaken-allow: secret
        return credential_module.AccessToken(token=token, expires_at=None)

    def refresh(request: object) -> object:
        requests.append(request)
        if len(requests) == 1:
            return transport_module.Response(
                401,
                request=request,
                json=canonical_error(401, "authentication_error"),
                headers={"request-id": "req_credential_stale", "retry-after-ms": "0"},
            )
        return transport_module.Response(200, request=request, json={})

    response = await _call_session_create(
        anthropic_module,
        transport_module,
        refresh,
        asynchronous=asynchronous,
        max_retries=1,
        raw=True,
        client_options={
            "api_key": None,
            "auth_token": None,
            "credentials": credentials,
        },
    )
    assert response.status_code == 200
    assert provider_calls == [False, True]
    assert len(requests) == 2
    assert [request.headers["authorization"] for request in requests] == [
        "Bearer provider-stale",
        "Bearer provider-fresh",
    ]
    assert all("x-api-key" not in request.headers for request in requests)
    assert [request.headers["x-stainless-retry-count"] for request in requests] == [
        "0",
        "1",
    ]
    for request in requests:
        assert sorted(
            item.strip()
            for item in request.headers["anthropic-beta"].split(",")
        ) == ["managed-agents-2026-04-01", "oauth-2025-04-20"]

    for static_kind in ("api_key", "auth_token"):
        shadowed_calls = []
        static_requests = []

        def shadowed_credentials(*, force_refresh: bool = False) -> object:
            shadowed_calls.append(force_refresh)
            return credential_module.AccessToken(
                token="must-not-be-used",  # awaken-allow: secret
                expires_at=None,
            )

        def accept(request: object) -> object:
            static_requests.append(request)
            return transport_module.Response(200, request=request, json={})

        static_options = {
            "api_key": "static-api" if static_kind == "api_key" else None,  # awaken-allow: secret
            "auth_token": "static-token" if static_kind == "auth_token" else None,  # awaken-allow: secret
            "credentials": shadowed_credentials,
        }
        response = await _call_session_create(
            anthropic_module,
            transport_module,
            accept,
            asynchronous=asynchronous,
            max_retries=0,
            raw=True,
            client_options=static_options,
        )
        assert response.status_code == 200
        assert shadowed_calls == []
        assert len(static_requests) == 1
        request = static_requests[0]
        if static_kind == "api_key":
            assert request.headers["x-api-key"] == "static-api"
            assert "authorization" not in request.headers
        else:
            assert request.headers["authorization"] == "Bearer static-token"
            assert "x-api-key" not in request.headers
        assert request.headers["anthropic-beta"] == "managed-agents-2026-04-01"

    # Causal graph: credential client -> with_options transport clone -> shared
    # TokenCache -> two Managed calls and one provider lookup. Supplying a new
    # credentials= provider forks that state while preserving default headers,
    # query, and the shared httpx transport. This runs through both official
    # sync and async client implementations under the one version authority
    # above.
    #
    # Decision table:
    # | derived client       | token cache | provider calls | wire token       |
    # | with_options(timeout)| shared      | one total      | parent-provider  |
    # | with_options(provider)| isolated   | one replacement| override-provider|
    parent_calls = []
    override_calls = []
    derived_requests = []

    def parent_credentials(*, force_refresh: bool = False) -> object:
        parent_calls.append(force_refresh)
        return credential_module.AccessToken(
            token="parent-provider",  # awaken-allow: secret
            expires_at=None,
        )

    def override_credentials(*, force_refresh: bool = False) -> object:
        override_calls.append(force_refresh)
        return credential_module.AccessToken(
            token="override-provider",  # awaken-allow: secret
            expires_at=None,
        )

    def accept_derived(request: object) -> object:
        derived_requests.append(request)
        return transport_module.Response(200, request=request, json={})

    common_options = {
        "api_key": None,
        "auth_token": None,
        "credentials": parent_credentials,
        "max_retries": 0,
        "default_headers": {"x-managed-parent": "preserved"},
        "default_query": {"inherited_query": "preserved"},
    }
    arguments = {"agent": "fixture", "environment_id": "fixture"}
    if asynchronous:
        async def async_accept_derived(request: object) -> object:
            return accept_derived(request)

        async with transport_module.AsyncClient(
            transport=transport_module.MockTransport(async_accept_derived)
        ) as http_client:
            async with anthropic_module.AsyncAnthropic(
                **common_options,
                http_client=http_client,
            ) as parent:
                clone = parent.with_options(timeout=1.0)
                overridden = parent.with_options(credentials=override_credentials)
                await parent.beta.sessions.create(**arguments)
                await clone.beta.sessions.create(**arguments)
                await overridden.beta.sessions.create(**arguments)
    else:
        with transport_module.Client(
            transport=transport_module.MockTransport(accept_derived)
        ) as http_client:
            with anthropic_module.Anthropic(
                **common_options,
                http_client=http_client,
            ) as parent:
                clone = parent.with_options(timeout=1.0)
                overridden = parent.with_options(credentials=override_credentials)
                parent.beta.sessions.create(**arguments)
                clone.beta.sessions.create(**arguments)
                overridden.beta.sessions.create(**arguments)

    assert parent_calls == [False]
    assert override_calls == [False]
    assert len(derived_requests) == 3
    assert [request.headers["authorization"] for request in derived_requests] == [
        "Bearer parent-provider",
        "Bearer parent-provider",
        "Bearer override-provider",
    ]
    assert all("x-api-key" not in request.headers for request in derived_requests)
    assert all(
        request.headers["x-managed-parent"] == "preserved"
        for request in derived_requests
    )
    assert all(
        request.url.params["inherited_query"] == "preserved"
        for request in derived_requests
    )

    environment_work = getattr(parent.beta.environments, "work", None)
    supports_poller = hasattr(environment_work, "poller")
    expected_poller = asynchronous and version in ENVIRONMENT_HELPER_VERSIONS
    assert supports_poller is expected_poller, (
        f"{version}: unreviewed environment-helper change point or client mode"
    )
    if asynchronous and supports_poller:
        # Helper composition graph: API-key parent -> public WorkPoller ->
        # scoped with_options client -> poll -> optional acknowledgement. The
        # helper credential must replace the parent API key while routing,
        # transport defaults, and helper telemetry survive. One claimed item
        # drives poll -> ack; the consumer then breaks with auto_stop=False,
        # proving the scoped request chain without introducing a synthetic stop
        # owner. A fatal authentication response exercises the other branch:
        # exact typed failure after one poll and no retry/ack side effect.
        #
        # Decision table:
        # | helper result | poll | ack | auth source    | terminal result      |
        # | claimed work  | one  | one | environment key| yielded typed work   |
        # | HTTP 401      | one  | zero| environment key| AuthenticationError  |
        helper_requests = []
        helper_work = {
            "id": "work_wire",
            "acknowledged_at": "2026-08-29T00:00:01Z",
            "created_at": "2026-08-29T00:00:00Z",
            "data": {"id": "session_wire", "type": "session"},
            "environment_id": "environment_wire",
            "latest_heartbeat_at": None,
            "metadata": {},
            "started_at": None,
            "state": "active",
            "stop_requested_at": None,
            "stopped_at": None,
            "type": "work",
        }

        async def accept_helper_request(request: object) -> object:
            helper_requests.append(request)
            return transport_module.Response(200, request=request, json=helper_work)

        async with transport_module.AsyncClient(
            transport=transport_module.MockTransport(accept_helper_request)
        ) as http_client:
            async with anthropic_module.AsyncAnthropic(
                api_key="parent-must-not-leak",  # awaken-allow: secret
                http_client=http_client,
                max_retries=0,
                default_headers={"x-parent-routing": "preserved"},
                default_query={"parent_query": "preserved"},
            ) as helper_parent:
                claimed = []
                async for work in helper_parent.beta.environments.work.poller(
                    environment_id="environment_wire",
                    environment_key="environment-helper",
                    worker_id="worker-helper",
                    block_ms=None,
                    auto_stop=False,
                ):
                    claimed.append(work)
                    break
        assert [work.id for work in claimed] == ["work_wire"]
        assert len(helper_requests) == 2
        assert [request.method for request in helper_requests] == ["GET", "POST"]
        assert [request.url.path for request in helper_requests] == [
            "/v1/environments/environment_wire/work/poll",
            "/v1/environments/environment_wire/work/work_wire/ack",
        ]
        assert all(
            request.headers["authorization"] == "Bearer environment-helper"
            for request in helper_requests
        )
        assert all("x-api-key" not in request.headers for request in helper_requests)
        assert all(
            request.headers["x-stainless-helper"] == "environments-work-poller"
            for request in helper_requests
        )
        assert helper_requests[0].headers["anthropic-worker-id"] == "worker-helper"
        assert all(
            request.headers["x-parent-routing"] == "preserved"
            for request in helper_requests
        )
        assert all(
            request.url.params["parent_query"] == "preserved"
            for request in helper_requests
        )

        denied_helper_requests = []

        async def deny_helper(request: object) -> object:
            denied_helper_requests.append(request)
            return transport_module.Response(
                401,
                request=request,
                json=canonical_error(401, "authentication_error"),
                headers={"request-id": "req_helper_denied"},
            )

        async with transport_module.AsyncClient(
            transport=transport_module.MockTransport(deny_helper)
        ) as http_client:
            async with anthropic_module.AsyncAnthropic(
                api_key="parent-must-not-leak",  # awaken-allow: secret
                http_client=http_client,
                max_retries=0,
            ) as denied_parent:
                try:
                    async for _ in denied_parent.beta.environments.work.poller(
                        environment_id="environment_wire",
                        environment_key="invalid-environment-helper",  # awaken-allow: secret
                        worker_id="worker-helper",
                        drain=True,
                        block_ms=None,
                    ):
                        raise AssertionError("denied helper yielded work")
                except anthropic_module.AuthenticationError as error:
                    assert error.__class__ is anthropic_module.AuthenticationError
                    assert error.status_code == 401
                    assert error.request_id == "req_helper_denied"
                else:
                    raise AssertionError("denied helper did not surface AuthenticationError")
        assert len(denied_helper_requests) == 1
        denied_request = denied_helper_requests[0]
        assert denied_request.headers["authorization"] == "Bearer invalid-environment-helper"
        assert "x-api-key" not in denied_request.headers
        assert denied_request.headers["x-stainless-helper"] == "environments-work-poller"


async def _exercise_middleware_contract_for_mode(
    anthropic_module: Any,
    transport_module: Any,
    *,
    asynchronous: bool,
) -> None:
    version = anthropic_module.__version__
    client_type = (
        anthropic_module.AsyncAnthropic
        if asynchronous
        else anthropic_module.Anthropic
    )
    supports_middleware = "middleware" in inspect.signature(client_type).parameters
    assert supports_middleware is (version in MIDDLEWARE_VERSIONS), (
        f"{version}: unreviewed middleware change point"
    )
    if not supports_middleware:
        return

    # Call-chain graph shared across sync and async clients:
    # generated Managed request -> outer middleware mutation -> inner
    # middleware -> httpx2 transport -> reverse unwind -> SDK retry -> the
    # complete chain again -> raw response. An ordinary middleware exception
    # instead terminates before transport and is never normalized or retried.
    #
    # Decision table:
    # | middleware outcome | retries | transport calls | caller result       |
    # | 500 then 200        | 1       | 2               | raw 200 response    |
    # | RuntimeError        | 2       | 0               | identical exception |
    events = []
    attempts = []

    def transport(request: object) -> object:
        retry = request.headers["x-stainless-retry-count"]
        attempts.append(request)
        events.append(f"transport:{retry}")
        assert request.headers["x-middleware-attempt"] == retry
        if len(attempts) == 1:
            return transport_module.Response(
                500,
                request=request,
                json=canonical_error(500, "api_error"),
                headers={"retry-after-ms": "0"},
            )
        return transport_module.Response(200, request=request, json={})

    if asynchronous:
        async def outer(request: object, call_next: Any) -> object:
            retry = request.retries_taken
            events.append(f"outer-before:{retry}")
            assert request.method == "post"
            assert request.url == "/v1/sessions?beta=true"
            response = await call_next(request.copy(headers={
                **request.headers,
                "x-middleware-attempt": str(retry),
            }))
            events.append(f"outer-after:{retry}:{response.status_code}")
            return response

        async def inner(request: object, call_next: Any) -> object:
            retry = request.retries_taken
            events.append(f"inner-before:{retry}")
            assert request.headers["x-middleware-attempt"] == str(retry)
            response = await call_next(request)
            events.append(f"inner-after:{retry}:{response.status_code}")
            return response
    else:
        def outer(request: object, call_next: Any) -> object:
            retry = request.retries_taken
            events.append(f"outer-before:{retry}")
            assert request.method == "post"
            assert request.url == "/v1/sessions?beta=true"
            response = call_next(request.copy(headers={
                **request.headers,
                "x-middleware-attempt": str(retry),
            }))
            events.append(f"outer-after:{retry}:{response.status_code}")
            return response

        def inner(request: object, call_next: Any) -> object:
            retry = request.retries_taken
            events.append(f"inner-before:{retry}")
            assert request.headers["x-middleware-attempt"] == str(retry)
            response = call_next(request)
            events.append(f"inner-after:{retry}:{response.status_code}")
            return response

    response = await _call_session_create(
        anthropic_module,
        transport_module,
        transport,
        asynchronous=asynchronous,
        max_retries=1,
        raw=True,
        client_options={"middleware": [outer, inner]},
    )
    assert response.status_code == 200
    assert len(attempts) == 2
    assert events == [
        "outer-before:0",
        "inner-before:0",
        "transport:0",
        "inner-after:0:500",
        "outer-after:0:500",
        "outer-before:1",
        "inner-before:1",
        "transport:1",
        "inner-after:1:200",
        "outer-after:1:200",
    ]

    sentinel = RuntimeError("middleware policy rejected request")
    rejected_transports = []

    def must_not_run(request: object) -> object:
        rejected_transports.append(request)
        return transport_module.Response(200, request=request, json={})

    if asynchronous:
        async def reject(_request: object, _call_next: Any) -> object:
            raise sentinel
    else:
        def reject(_request: object, _call_next: Any) -> object:
            raise sentinel

    try:
        await _call_session_create(
            anthropic_module,
            transport_module,
            must_not_run,
            asynchronous=asynchronous,
            max_retries=2,
            client_options={"middleware": [reject]},
        )
    except RuntimeError as error:
        assert error is sentinel
    else:
        raise AssertionError("Python SDK accepted a rejected middleware request")
    assert rejected_transports == []


def exercise_error_and_retry_contract(anthropic_module: Any, transport_module: Any) -> None:
    asyncio.run(_exercise_error_and_retry_contract_for_mode(
        anthropic_module,
        transport_module,
        asynchronous=False,
    ))


async def exercise_async_error_and_retry_contract(
    anthropic_module: Any,
    transport_module: Any,
) -> None:
    await _exercise_error_and_retry_contract_for_mode(
        anthropic_module,
        transport_module,
        asynchronous=True,
    )
