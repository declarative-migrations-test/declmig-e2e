#!/usr/bin/env python3
"""Validate an instantiated declmig-e2e repository before any database work."""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
from pathlib import Path
from typing import Any

SHA40 = re.compile(r"^[0-9a-f]{40}$")
SECRET_PATTERNS = (
    re.compile(r"gh[pousr]_[A-Za-z0-9]{20,}"),
    re.compile(r"github_pat_[A-Za-z0-9_]{20,}"),
    re.compile(r"lin_api_[A-Za-z0-9]{20,}"),
    re.compile(r"cfat_[A-Za-z0-9_-]{20,}"),
    re.compile(r"BEGIN (?:RSA|OPENSSH|EC) PRIVATE KEY"),
)


class ConfigError(ValueError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ConfigError(message)


def load_object(path: Path) -> dict[str, Any]:
    data = json.loads(path.read_text(encoding="utf-8"))
    require(isinstance(data, dict), f"{path} must contain a JSON object")
    return data


def walk_strings(value: Any) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [text for item in value for text in walk_strings(item)]
    if isinstance(value, dict):
        return [text for key, item in value.items() for text in (*walk_strings(key), *walk_strings(item))]
    return []


def main() -> int:
    try:
        config_path = Path("config/repository.json")
        config = load_object(config_path)
        require(config.get("schema_version") == 1, "repository schema_version must be 1")
        repository = config.get("repository")
        require(repository in {"declarative-migrations/declmig-e2e", "declarative-migrations-test/declmig-e2e"}, "unexpected aggregate repository")
        require(config.get("source_repository") == "declarative-migrations/declarative-postgres-migrate.rs", "source repository mismatch")
        require(config.get("production_credentials_allowed") is False, "production credentials must be forbidden")
        require(config.get("required_jobs") == ["contract", "postgres-smoke"], "required job set mismatch")

        mode = config.get("mode")
        if repository == "declarative-migrations/declmig-e2e":
            require(mode == "stable-promotion-orchestrator", "production mode mismatch")
            require(config.get("destructive_targets_allowed") is False, "production aggregate cannot accept destructive targets")
            require(config.get("test_evidence_required_before_release") is True, "test evidence must precede release")
            require(config.get("evidence_producer") == "declarative-migrations-test/declmig-e2e", "test evidence producer mismatch")
        else:
            require(mode == "candidate-and-destructive-fleet-orchestrator", "test mode mismatch")
            require(config.get("destructive_targets_allowed") is True, "test aggregate must permit disposable destructive targets")
            require(config.get("required_destructive_target_class") == "ephemeral-or-explicitly-disposable", "destructive target class mismatch")
            require(config.get("production_database_targets_allowed") is False, "production databases are forbidden")
            require(config.get("evidence_consumer") == "declarative-migrations/declmig-e2e", "evidence consumer mismatch")

        github_repository = os.environ.get("GITHUB_REPOSITORY")
        if github_repository:
            require(github_repository == repository, f"config is for {repository}, runner is {github_repository}")

        pin_path = Path(str(config.get("source_pin_file")))
        pin = load_object(pin_path)
        require(pin.get("schema_version") == 1, "source pin schema_version must be 1")
        require(pin.get("source_repository") == config.get("source_repository"), "source pin repository mismatch")
        commit = pin.get("source_commit")
        require(isinstance(commit, str) and SHA40.fullmatch(commit) is not None, "source_commit must be a full lowercase SHA")

        fixture_paths = [Path("fixtures/current.sql"), Path("fixtures/desired.sql")]
        for path in fixture_paths:
            require(path.is_file() and path.stat().st_size > 0, f"missing fixture {path}")

        for text in walk_strings(config) + walk_strings(pin):
            for pattern in SECRET_PATTERNS:
                require(pattern.search(text) is None, "configuration contains secret-like material")

        summary = {
            "repository": repository,
            "mode": mode,
            "source_repository": pin["source_repository"],
            "source_commit": commit,
            "config_sha256": hashlib.sha256(config_path.read_bytes()).hexdigest(),
            "pin_sha256": hashlib.sha256(pin_path.read_bytes()).hexdigest(),
        }
        Path("artifacts").mkdir(exist_ok=True)
        Path("artifacts/config-validation.json").write_text(
            json.dumps(summary, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print(json.dumps(summary, sort_keys=True))
        return 0
    except (OSError, json.JSONDecodeError, ConfigError) as exc:
        print(f"declmig repository config invalid: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
