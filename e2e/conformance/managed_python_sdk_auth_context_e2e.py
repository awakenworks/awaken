from __future__ import annotations

import os

import anthropic

from managed_python_sdk_request_contract import (
    projects_workspace_response_context,
)


BASE_URL = os.environ["AWAKEN_MANAGED_BASE_URL"]
ADMIN_TOKEN = os.environ["AWAKEN_MANAGED_ADMIN_TOKEN"]
RESTRICTED_TOKEN = os.environ["AWAKEN_MANAGED_RESTRICTED_TOKEN"]
WORKSPACE_ID = os.environ["AWAKEN_MANAGED_WORKSPACE_ID"]


def record_request_id(seen: set[str], value: str | None, label: str) -> str:
    assert value and value.startswith("req_"), f"{label}: generated request-id"
    assert value not in seen, f"{label}: request-id is unique"
    seen.add(value)
    return value


def main() -> None:
    version = anthropic.__version__
    label = f"python:{version}"
    projects_workspace = projects_workspace_response_context(version)
    request_ids: set[str] = set()
    denied_name = f"python-authz-denied-{version}"

    # Test design: every_python_sdk_composes_with_the_real_authentication_boundary
    #
    # Cause/effect graph:
    # exact reviewed wheel + invalid x-api-key -> real self-managed IAM guard
    # -> canonical unscoped 401 -> exact AuthenticationError, fresh request id,
    # no Workspace; the same wheel + restricted Bearer + Vault create -> scoped
    # 403 -> exact PermissionDeniedError and token Workspace; admin Bearer ->
    # decoded page and exact Workspace. A final admin read proves denial wrote
    # no resource.
    #
    # Decision table:
    # | credential | action | Python result         | Workspace | side effect |
    # | invalid    | read   | AuthenticationError  | absent    | none        |
    # | restricted | write  | PermissionDeniedError| exact     | none        |
    # | admin      | read   | Vault page           | exact     | none        |
    #
    # Constraints: the wheel is selected only by the generated Python anchor
    # matrix; retries are disabled; no caller-supplied Workspace is accepted as
    # evidence; the fresh deployment's final page is the persistence authority.
    with anthropic.Anthropic(
        api_key=f"invalid-{version}",  # awaken-allow: secret
        base_url=BASE_URL,
        max_retries=0,
    ) as invalid:
        try:
            invalid.beta.vaults.list()
        except anthropic.AuthenticationError as error:
            assert error.__class__ is anthropic.AuthenticationError
            assert error.status_code == 401
            assert error.body["type"] == "error"
            assert error.body["error"]["type"] == "authentication_error"
            request_id = record_request_id(
                request_ids,
                error.response.headers.get("request-id"),
                f"{label}: unauthenticated response",
            )
            assert error.request_id == request_id
            assert error.response.headers.get("anthropic-workspace-id") is None
            assert hasattr(error, "workspace_id") is projects_workspace
            if projects_workspace:
                assert error.workspace_id is None
        else:
            raise AssertionError(f"{label}: invalid credential was accepted")

    with anthropic.Anthropic(
        api_key=None,
        auth_token=ADMIN_TOKEN,
        base_url=BASE_URL,
        max_retries=0,
    ) as admin:
        raw_page = admin.beta.vaults.with_raw_response.list()
        record_request_id(
            request_ids,
            raw_page.headers.get("request-id"),
            f"{label}: authenticated response",
        )
        assert raw_page.headers.get("anthropic-workspace-id") == WORKSPACE_ID
        page = raw_page.parse()
        assert isinstance(page.data, list)

    with anthropic.Anthropic(
        api_key=None,
        auth_token=RESTRICTED_TOKEN,
        base_url=BASE_URL,
        max_retries=0,
    ) as restricted:
        try:
            restricted.beta.vaults.create(display_name=denied_name)
        except anthropic.PermissionDeniedError as error:
            assert error.__class__ is anthropic.PermissionDeniedError
            assert error.status_code == 403
            assert error.body["type"] == "error"
            assert error.body["error"]["type"] == "permission_error"
            request_id = record_request_id(
                request_ids,
                error.response.headers.get("request-id"),
                f"{label}: denied response",
            )
            assert error.request_id == request_id
            assert error.response.headers.get("anthropic-workspace-id") == WORKSPACE_ID
            assert hasattr(error, "workspace_id") is projects_workspace
            if projects_workspace:
                assert error.workspace_id == WORKSPACE_ID
        else:
            raise AssertionError(f"{label}: restricted mutation was accepted")

    with anthropic.Anthropic(
        api_key=None,
        auth_token=ADMIN_TOKEN,
        base_url=BASE_URL,
        max_retries=0,
    ) as admin:
        raw_verification = admin.beta.vaults.with_raw_response.list()
        record_request_id(
            request_ids,
            raw_verification.headers.get("request-id"),
            f"{label}: post-denial verification",
        )
        assert raw_verification.headers.get("anthropic-workspace-id") == WORKSPACE_ID
        verification = raw_verification.parse()
        assert all(vault.display_name != denied_name for vault in verification.data)

    assert len(request_ids) == 4
    print(f"PYTHON SDK AUTH CONTEXT PASS {version}: 401/403/200 and no denied side effect")


if __name__ == "__main__":
    main()
