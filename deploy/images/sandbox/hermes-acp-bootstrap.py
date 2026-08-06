#!/opt/awaken-python/bin/python3
"""Prepare one isolated Hermes ACP home without making startup network-bound.

Hermes resolves model metadata while constructing an ACP session.  A fresh
container has no models.dev disk cache, so `session/new` otherwise performs an
unrelated network request before the managed model can be selected through ACP.
The Worker already supplies the exact managed model in `HERMES_MODEL`; project
that value into Hermes' per-session config and a minimal fresh metadata entry,
then execute the immutable, image-installed adapter.
"""

from __future__ import annotations

import json
import os
import stat
import sys
import tempfile
from pathlib import Path

import yaml


REAL_ADAPTER = "/opt/awaken-python/bin/hermes-acp.real"
DEFAULT_CONTEXT_WINDOW = 131_072
DEFAULT_OUTPUT_TOKENS = 8_192


def _positive_int(name: str, default: int) -> int:
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    try:
        value = int(raw)
    except ValueError:
        return default
    return value if value > 0 else default


def _atomic_write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(descriptor, stat.S_IRUSR | stat.S_IWUSR)
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def _load_mapping(path: Path, *, yaml_document: bool) -> dict:
    try:
        text = path.read_text(encoding="utf-8")
        value = yaml.safe_load(text) if yaml_document else json.loads(text)
    except (OSError, ValueError, yaml.YAMLError):
        return {}
    return value if isinstance(value, dict) else {}


def prepare(home: Path, model: str) -> None:
    config_path = home / "config.yaml"
    config = _load_mapping(config_path, yaml_document=True)
    model_config = config.get("model")
    if not isinstance(model_config, dict):
        model_config = {}
        config["model"] = model_config
    model_config["default"] = model
    model_config["provider"] = "deepseek"
    _atomic_write(config_path, yaml.safe_dump(config, sort_keys=False))

    cache_path = home / "models_dev_cache.json"
    cache = _load_mapping(cache_path, yaml_document=False)
    provider = cache.setdefault("deepseek", {})
    if not isinstance(provider, dict):
        provider = {}
        cache["deepseek"] = provider
    models = provider.setdefault("models", {})
    if not isinstance(models, dict):
        models = {}
        provider["models"] = models
    models.setdefault(
        model,
        {
            "tool_call": True,
            "attachment": False,
            "reasoning": any(token in model.lower() for token in ("reason", "r1")),
            "limit": {
                "context": _positive_int(
                    "HERMES_CONTEXT_WINDOW", DEFAULT_CONTEXT_WINDOW
                ),
                "output": _positive_int(
                    "HERMES_MAX_OUTPUT_TOKENS", DEFAULT_OUTPUT_TOKENS
                ),
            },
        },
    )
    _atomic_write(cache_path, json.dumps(cache, separators=(",", ":")))

    # Hermes also has a provider-unaware OpenRouter metadata fallback.  Some
    # initialization call sites omit the already-resolved provider, so seed the
    # same managed model there as well; otherwise those paths still perform a
    # second implicit network request despite the models.dev cache above.
    openrouter_cache_path = home / "cache" / "openrouter_model_metadata.json"
    openrouter_cache = _load_mapping(openrouter_cache_path, yaml_document=False)
    openrouter_cache.setdefault(
        model,
        {
            "context_length": _positive_int(
                "HERMES_CONTEXT_WINDOW", DEFAULT_CONTEXT_WINDOW
            ),
            "max_completion_tokens": _positive_int(
                "HERMES_MAX_OUTPUT_TOKENS", DEFAULT_OUTPUT_TOKENS
            ),
            "name": model,
            "pricing": {},
        },
    )
    _atomic_write(
        openrouter_cache_path,
        json.dumps(openrouter_cache, separators=(",", ":")),
    )


def self_test() -> int:
    with tempfile.TemporaryDirectory(prefix="awaken-hermes-bootstrap-") as directory:
        home = Path(directory)
        os.environ["HERMES_CONTEXT_WINDOW"] = "65536"
        prepare(home, "managed-model")
        config = yaml.safe_load((home / "config.yaml").read_text(encoding="utf-8"))
        cache = json.loads((home / "models_dev_cache.json").read_text(encoding="utf-8"))
        assert config["model"] == {
            "default": "managed-model",
            "provider": "deepseek",
        }
        entry = cache["deepseek"]["models"]["managed-model"]
        assert entry["tool_call"] is True
        assert entry["limit"]["context"] == 65536
        openrouter = json.loads(
            (home / "cache" / "openrouter_model_metadata.json").read_text(
                encoding="utf-8"
            )
        )
        assert openrouter["managed-model"]["context_length"] == 65536
        assert stat.S_IMODE((home / "config.yaml").stat().st_mode) == 0o600
    return 0


def main() -> int:
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    model = os.environ.get("HERMES_MODEL", "").strip()
    if model:
        home = Path(
            os.environ.get("HERMES_HOME", "").strip()
            or (Path.home() / ".hermes")
        )
        prepare(home, model)
    os.execv(REAL_ADAPTER, [REAL_ADAPTER, *sys.argv[1:]])
    return 127


if __name__ == "__main__":
    raise SystemExit(main())
