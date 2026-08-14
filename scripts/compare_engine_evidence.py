#!/usr/bin/env python3
"""Compare immutable PostgreSQL and CockroachDB aggregate evidence."""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path
from typing import Any


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    require(isinstance(value, dict), f"{path} must contain a JSON object")
    return value


def verify_evidence(directory: Path, engine_kind: str) -> dict[str, Any]:
    evidence = load_json(directory / "evidence.json")
    require(evidence.get("schema_version") == 1, f"{engine_kind} evidence schema mismatch")
    require(evidence.get("engine_kind") == engine_kind, f"{engine_kind} evidence identity mismatch")
    require(evidence.get("result") == "passed", f"{engine_kind} evidence is not passing")
    artifacts = evidence.get("artifacts")
    require(isinstance(artifacts, dict), f"{engine_kind} artifact digest map missing")
    for name, expected in artifacts.items():
        path = directory / name
        require(path.is_file(), f"{engine_kind} artifact missing: {name}")
        require(digest(path) == expected, f"{engine_kind} artifact digest mismatch: {name}")
    return evidence


def plan_ops(path: Path) -> list[str]:
    plan = load_json(path)
    changes = plan.get("changes")
    require(isinstance(changes, list), f"{path} changes must be a list")
    ops: list[str] = []
    for change in changes:
        require(isinstance(change, dict), f"{path} contains a non-object change")
        op = change.get("op")
        require(isinstance(op, str), f"{path} change is missing op")
        if op == "create_function":
            kind = change.get("kind")
            require(kind in {"function", "procedure"}, f"{path} routine change has an invalid kind")
            if kind == "procedure":
                op = "create_procedure"
        ops.append(op)
    return ops


def require_empty_plan(path: Path) -> None:
    require(plan_ops(path) == [], f"{path} must contain an empty converged plan")


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: compare_engine_evidence.py POSTGRES_DIR COCKROACH_DIR OUTPUT", file=sys.stderr)
        return 2

    postgres_dir, cockroach_dir, output_path = map(Path, sys.argv[1:])
    try:
        postgres = verify_evidence(postgres_dir, "postgres")
        cockroach = verify_evidence(cockroach_dir, "cockroach")
        require(postgres["source_commit"] == cockroach["source_commit"], "source commits differ")
        require(postgres["workflow_commit"] == cockroach["workflow_commit"], "workflow commits differ")

        postgres_ops = plan_ops(postgres_dir / "plan.json")
        cockroach_ops = plan_ops(cockroach_dir / "plan.json")
        require(postgres_ops == cockroach_ops, "portable plan operation sequence differs")
        require(
            {
                "add_column",
                "create_function",
                "create_procedure",
                "create_table",
                "create_trigger",
                "create_view",
            }.issubset(postgres_ops),
            "portable plan omits an expected migration operation",
        )

        for directory in (postgres_dir, cockroach_dir):
            require_empty_plan(directory / "post-apply.json")
            require_empty_plan(directory / "post-replay.json")

        postgres_signature = postgres_dir / "portable-signature.txt"
        cockroach_signature = cockroach_dir / "portable-signature.txt"
        require(
            postgres_signature.read_bytes() == cockroach_signature.read_bytes(),
            "portable catalog signatures differ",
        )

        report = {
            "schema_version": 1,
            "result": "passed",
            "source_commit": postgres["source_commit"],
            "workflow_commit": postgres["workflow_commit"],
            "engines": [postgres["engine"], cockroach["engine"]],
            "plan_operations": postgres_ops,
            "portable_signature_sha256": digest(postgres_signature),
            "postgres_evidence_sha256": digest(postgres_dir / "evidence.json"),
            "cockroach_evidence_sha256": digest(cockroach_dir / "evidence.json"),
        }
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps(report, sort_keys=True))
        return 0
    except (OSError, KeyError, json.JSONDecodeError, ValueError) as exc:
        print(f"dual-engine parity verification failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
