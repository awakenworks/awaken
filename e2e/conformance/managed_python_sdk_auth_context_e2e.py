from __future__ import annotations

import importlib
import inspect
import json
import os
import tempfile
from pathlib import Path

import anthropic

from managed_python_sdk_request_contract import (
    CREDENTIAL_PROVIDER_VERSIONS,
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
    # no resource. Wheels exposing the dynamic provider surface additionally
    # resolve one AccessToken and carry the same authenticated facts through a
    # real request. The same wheels also load an in-memory user-OAuth config and
    # private token file, then a named profile in an isolated config directory,
    # through the SDK's own provider chain; 0.92 is the explicit
    # pre-provider/config/profile change point.
    #
    # Decision table:
    # | credential | action | Python result         | Workspace | side effect |
    # | invalid    | read   | AuthenticationError  | absent    | none        |
    # | restricted | write  | PermissionDeniedError| exact     | none        |
    # | admin      | read   | Vault page           | exact     | none        |
    # | provider   | read   | Vault page, one lookup| exact    | none        |
    # | config     | read   | Vault page, file token| exact    | none        |
    # | profile    | read   | Vault page, named file| exact    | none        |
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

    supports_credentials = "credentials" in inspect.signature(anthropic.Anthropic).parameters
    assert supports_credentials is (version in CREDENTIAL_PROVIDER_VERSIONS), (
        f"{label}: unreviewed credential-provider change point"
    )
    supports_config = "config" in inspect.signature(anthropic.Anthropic).parameters
    assert supports_config is supports_credentials, (
        f"{label}: provider/config capability boundary diverged"
    )
    extended_auth_request_count = 0
    if supports_credentials:
        credential_module = importlib.import_module("anthropic.lib.credentials")
        provider_calls: list[bool] = []

        def credentials(*, force_refresh: bool = False) -> object:
            provider_calls.append(force_refresh)
            return credential_module.AccessToken(token=ADMIN_TOKEN, expires_at=None)

        with anthropic.Anthropic(
            api_key=None,
            auth_token=None,
            credentials=credentials,
            base_url=BASE_URL,
            max_retries=0,
        ) as provider:
            raw_provider_page = provider.beta.vaults.with_raw_response.list()
            record_request_id(
                request_ids,
                raw_provider_page.headers.get("request-id"),
                f"{label}: dynamic-provider response",
            )
            assert raw_provider_page.headers.get("anthropic-workspace-id") == WORKSPACE_ID
            provider_page = raw_provider_page.parse()
            assert isinstance(provider_page.data, list)
        assert provider_calls == [False]
        extended_auth_request_count = 1

        with tempfile.TemporaryDirectory(prefix="awaken-python-sdk-config-") as directory:
            credentials_path = Path(directory) / "credentials.json"
            credentials_path.write_text(json.dumps({
                "version": "1.0",
                "type": "oauth_token",
                "access_token": ADMIN_TOKEN,
            }), encoding="utf-8")
            credentials_path.chmod(0o600)
            with anthropic.Anthropic(
                api_key=None,
                auth_token=None,
                config={
                    "authentication": {
                        "type": "user_oauth",
                        "credentials_path": str(credentials_path),
                    },
                    "base_url": BASE_URL,
                    "workspace_id": WORKSPACE_ID,
                },
                # Keep the credential-bearing request pinned to the fixture.
                # Python's config-host adoption is an upstream client concern;
                # this cross-layer test owns config auth and Workspace headers.
                base_url=BASE_URL,
                max_retries=0,
            ) as configured:
                raw_config_page = configured.beta.vaults.with_raw_response.list()
                record_request_id(
                    request_ids,
                    raw_config_page.headers.get("request-id"),
                    f"{label}: configured-credential response",
                )
                assert raw_config_page.headers.get("anthropic-workspace-id") == WORKSPACE_ID
                config_page = raw_config_page.parse()
                assert isinstance(config_page.data, list)

            profile_root = Path(directory) / "profile-root"
            profile_config_directory = profile_root / "configs"
            profile_config_directory.mkdir(parents=True)
            profile_credentials_path = profile_root / "credentials.json"
            profile_credentials_path.write_text(json.dumps({
                "version": "1.0",
                "type": "oauth_token",
                "access_token": ADMIN_TOKEN,
            }), encoding="utf-8")
            profile_credentials_path.chmod(0o600)
            (profile_config_directory / "fixture.json").write_text(json.dumps({
                "version": "1.0",
                "authentication": {
                    "type": "user_oauth",
                    "credentials_path": str(profile_credentials_path),
                },
                "base_url": BASE_URL,
                "workspace_id": WORKSPACE_ID,
            }), encoding="utf-8")
            previous_config_directory = os.environ.get("ANTHROPIC_CONFIG_DIR")
            os.environ["ANTHROPIC_CONFIG_DIR"] = str(profile_root)
            try:
                with anthropic.Anthropic(
                    profile="fixture",
                    # Pin the credential-bearing request to the fixture even
                    # if profile host selection regresses upstream.
                    base_url=BASE_URL,
                    max_retries=0,
                ) as profiled:
                    raw_profile_page = profiled.beta.vaults.with_raw_response.list()
                    record_request_id(
                        request_ids,
                        raw_profile_page.headers.get("request-id"),
                        f"{label}: profile-credential response",
                    )
                    assert (
                        raw_profile_page.headers.get("anthropic-workspace-id")
                        == WORKSPACE_ID
                    )
                    profile_page = raw_profile_page.parse()
                    assert isinstance(profile_page.data, list)
            finally:
                if previous_config_directory is None:
                    os.environ.pop("ANTHROPIC_CONFIG_DIR", None)
                else:
                    os.environ["ANTHROPIC_CONFIG_DIR"] = previous_config_directory
        extended_auth_request_count = 3

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

    assert len(request_ids) == 4 + extended_auth_request_count
    auth_modes = "static/provider/config/profile" if supports_credentials else "static"
    print(
        f"PYTHON SDK AUTH CONTEXT PASS {version}: "
        f"{auth_modes} 401/403/200 and no denied side effect"
    )


if __name__ == "__main__":
    main()
