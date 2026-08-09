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
DEFAULT_CONTEXT_WINDOW = 1_000_000
DEFAULT_OUTPUT_TOKENS = 384_000


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
    # The current frozen Session projection is authoritative even when an
    # earlier attempt cached the same model id with different limits.
    models[model] = {
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
    }
    _atomic_write(cache_path, json.dumps(cache, separators=(",", ":")))

    # Hermes also has a provider-unaware OpenRouter metadata fallback.  Some
    # initialization call sites omit the already-resolved provider, so seed the
    # same managed model there as well; otherwise those paths still perform a
    # second implicit network request despite the models.dev cache above.
    openrouter_cache_path = home / "cache" / "openrouter_model_metadata.json"
    openrouter_cache = _load_mapping(openrouter_cache_path, yaml_document=False)
    openrouter_cache[model] = {
        "context_length": _positive_int(
            "HERMES_CONTEXT_WINDOW", DEFAULT_CONTEXT_WINDOW
        ),
        "max_completion_tokens": _positive_int(
            "HERMES_MAX_OUTPUT_TOKENS", DEFAULT_OUTPUT_TOKENS
        ),
        "name": model,
        "pricing": {},
    }
    _atomic_write(
        openrouter_cache_path,
        json.dumps(openrouter_cache, separators=(",", ":")),
    )


def self_test() -> int:
    # Bootstrap FMECA/cause-effect decision table:
    # C1=frozen model/positive limits; C2=missing, invalid, or non-positive
    # limits; C3=valid unrelated config/cache; C4=stale entry for the same model.
    # Effects: E1=project exact frozen values; E2=use safe positive defaults;
    # E3=preserve unrelated state; E4=replace stale same-model metadata; E5=all
    # managed files are atomically published mode 0600. Rules H1 C1+C3=>E1+E3+E5;
    # H2 C2=>E2+E5; H3 C1+C4=>E1+E4+E5.
    with tempfile.TemporaryDirectory(prefix="awaken-hermes-bootstrap-") as directory:
        home = Path(directory)
        (home / "config.yaml").write_text("unrelated: keep\n", encoding="utf-8")
        (home / "models_dev_cache.json").write_text(
            json.dumps(
                {
                    "other": {"models": {"other-model": {"keep": True}}},
                    "deepseek": {
                        "models": {"managed-model": {"limit": {"context": 1}}}
                    },
                }
            ),
            encoding="utf-8",
        )
        (home / "cache").mkdir()
        (home / "cache" / "openrouter_model_metadata.json").write_text(
            json.dumps(
                {
                    "other-model": {"keep": True},
                    "managed-model": {"context_length": 1},
                }
            ),
            encoding="utf-8",
        )
        previous = {
            name: os.environ.get(name)
            for name in ("HERMES_CONTEXT_WINDOW", "HERMES_MAX_OUTPUT_TOKENS")
        }
        try:
            os.environ["HERMES_CONTEXT_WINDOW"] = "65536"
            os.environ["HERMES_MAX_OUTPUT_TOKENS"] = "8192"
            prepare(home, "managed-model")
            config = yaml.safe_load((home / "config.yaml").read_text(encoding="utf-8"))
            cache = json.loads(
                (home / "models_dev_cache.json").read_text(encoding="utf-8")
            )
            assert config["unrelated"] == "keep", "H1/E3"
            assert config["model"] == {
                "default": "managed-model",
                "provider": "deepseek",
            }, "H1/E1"
            assert cache["other"]["models"]["other-model"]["keep"] is True, "H1/E3"
            entry = cache["deepseek"]["models"]["managed-model"]
            assert entry["limit"] == {"context": 65536, "output": 8192}, "H3/E4"
            openrouter_path = home / "cache" / "openrouter_model_metadata.json"
            openrouter = json.loads(openrouter_path.read_text(encoding="utf-8"))
            assert openrouter["other-model"]["keep"] is True, "H1/E3"
            assert openrouter["managed-model"]["context_length"] == 65536, "H3/E4"

            os.environ["HERMES_CONTEXT_WINDOW"] = "invalid"
            os.environ["HERMES_MAX_OUTPUT_TOKENS"] = "0"
            prepare(home, "fallback-model")
            cache = json.loads(
                (home / "models_dev_cache.json").read_text(encoding="utf-8")
            )
            fallback = cache["deepseek"]["models"]["fallback-model"]["limit"]
            assert fallback == {
                "context": DEFAULT_CONTEXT_WINDOW,
                "output": DEFAULT_OUTPUT_TOKENS,
            }, "H2/E2"
            for path in (
                home / "config.yaml",
                home / "models_dev_cache.json",
                openrouter_path,
            ):
                assert stat.S_IMODE(path.stat().st_mode) == 0o600, f"H1-H3/E5: {path}"
        finally:
            for name, value in previous.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
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
