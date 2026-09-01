from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SCRIPT = Path(__file__).parents[1] / "scripts" / "compare_engine_evidence.py"
SPEC = importlib.util.spec_from_file_location("compare_engine_evidence", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
COMPARE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COMPARE)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class CompareEngineEvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.postgres = self.root / "postgres"
        self.cockroach = self.root / "cockroach"
        self.output = self.root / "parity.json"
        self._write_engine(self.postgres, "postgres")
        self._write_engine(self.cockroach, "cockroach")

    def tearDown(self) -> None:
        self.temp.cleanup()

    def _write_engine(self, directory: Path, engine_kind: str) -> None:
        directory.mkdir()
        files = {
            "plan.json": {
                "changes": [
                    {"op": "create_table", "table": {"schema": "app", "name": "projects"}},
                    {"op": "add_column", "table": {"schema": "app", "name": "accounts"}},
                    {"op": "create_table", "table": {"schema": "app", "name": "account_audit"}},
                    {"op": "create_view", "view": {"schema": "app", "name": "named_accounts"}},
                    {
                        "op": "create_function",
                        "kind": "function",
                        "key": "app.audit_account_update()",
                    },
                    {
                        "op": "create_function",
                        "kind": "procedure",
                        "key": "app.set_account_display_name(bigint, text)",
                    },
                    {"op": "create_trigger", "trigger": {"schema": "app", "name": "accounts_audit"}},
                ]
            },
            "post-apply.json": {"changes": []},
            "post-replay.json": {"changes": []},
            "evolution-plan.json": {
                "changes": [
                    {"op": "add_enum_value"},
                    {"op": "alter_sequence"},
                    {"op": "set_default"},
                    {"op": "set_default"},
                    {"op": "set_not_null"},
                    {"op": "add_column"},
                    {"op": "add_column"},
                    {"op": "add_constraint"},
                    {"op": "drop_index"},
                    {"op": "create_index"},
                    {"op": "drop_view"},
                    {"op": "create_view"},
                    {"op": "create_function", "kind": "function"},
                    {"op": "create_function", "kind": "function"},
                    {"op": "create_function", "kind": "procedure"},
                ]
            },
            "evolution-post-apply.json": {"changes": []},
            "evolution-post-replay.json": {"changes": []},
        }
        for name, value in files.items():
            (directory / name).write_text(json.dumps(value) + "\n", encoding="utf-8")
        (directory / "portable-signature.txt").write_text(
            "column|accounts|email|text|NO\nindex|projects|projects_owner_account_id_idx\n",
            encoding="utf-8",
        )
        (directory / "evolution-portable-signature.txt").write_text(
            "column|total_cents|bigint|YES|ALWAYS\nenum|pending,paid,fulfilled\n",
            encoding="utf-8",
        )
        generated_artifacts = {
            "catalog-assertions.txt": "catalog\n",
            "data-assertions.txt": "data\n",
            "evolution-catalog-assertions.txt": "catalog\n",
            "evolution-constraint-rejection.txt": "constraint rejected\n",
            "evolution-data-assertions.txt": "data\n",
            "evolution-enum-default-insert.txt": "inserted\n",
            "evolution-plan.sql": "-- plan\n",
            "evolution-preserved-order-id.txt": "1\n",
            "evolution-procedure-execution.txt": "CALL\n",
            "plan.sql": "-- plan\n",
            "preserved-account-id.txt": "1\n",
            "procedure-execution.txt": "CALL\n",
        }
        for name, value in generated_artifacts.items():
            (directory / name).write_text(value, encoding="utf-8")
        artifact_names = (
            *files.keys(),
            *generated_artifacts.keys(),
            "portable-signature.txt",
            "evolution-portable-signature.txt",
        )
        evidence = {
            "schema_version": 1,
            "result": "passed",
            "source_commit": "a" * 40,
            "workflow_commit": "b" * 40,
            "engine_kind": engine_kind,
            "engine": f"{engine_kind}:test@sha256:{'c' * 64}",
            "artifacts": {name: digest(directory / name) for name in artifact_names},
        }
        (directory / "evidence.json").write_text(
            json.dumps(evidence, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )

    def _refresh_digest(self, directory: Path, name: str) -> None:
        evidence_path = directory / "evidence.json"
        evidence = json.loads(evidence_path.read_text(encoding="utf-8"))
        evidence["artifacts"][name] = digest(directory / name)
        evidence_path.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    def _run_compare(self) -> int:
        argv = [str(SCRIPT), str(self.postgres), str(self.cockroach), str(self.output)]
        with mock.patch.object(sys, "argv", argv):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return COMPARE.main()

    def test_accepts_matching_integrity_checked_evidence(self) -> None:
        self.assertEqual(self._run_compare(), 0)
        report = json.loads(self.output.read_text(encoding="utf-8"))
        self.assertEqual(report["result"], "passed")
        self.assertEqual(
            report["plan_operations"],
            [
                "create_table",
                "add_column",
                "create_table",
                "create_view",
                "create_function",
                "create_procedure",
                "create_trigger",
            ],
        )
        self.assertEqual(
            report["evolution_plan_operations"],
            [
                "add_enum_value",
                "alter_sequence",
                "set_default",
                "set_default",
                "set_not_null",
                "add_column",
                "add_column",
                "add_constraint",
                "drop_index",
                "create_index",
                "drop_view",
                "create_view",
                "create_function",
                "create_function",
                "create_procedure",
            ],
        )

    def test_rejects_tampered_artifact(self) -> None:
        (self.cockroach / "plan.json").write_text('{"changes": []}\n', encoding="utf-8")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_portable_plan_operation_drift(self) -> None:
        (self.cockroach / "plan.json").write_text(
            '{"changes": [{"op": "create_table"}]}\n',
            encoding="utf-8",
        )
        self._refresh_digest(self.cockroach, "plan.json")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_portable_catalog_drift(self) -> None:
        (self.cockroach / "portable-signature.txt").write_text(
            "column|accounts|email|string|NO\n",
            encoding="utf-8",
        )
        self._refresh_digest(self.cockroach, "portable-signature.txt")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_portable_evolution_plan_drift(self) -> None:
        (self.cockroach / "evolution-plan.json").write_text(
            '{"changes": [{"op": "add_column"}]}\n',
            encoding="utf-8",
        )
        self._refresh_digest(self.cockroach, "evolution-plan.json")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_portable_evolution_catalog_drift(self) -> None:
        (self.cockroach / "evolution-portable-signature.txt").write_text(
            "column|total_cents|string|YES|ALWAYS\n",
            encoding="utf-8",
        )
        self._refresh_digest(self.cockroach, "evolution-portable-signature.txt")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_missing_required_artifact_digest(self) -> None:
        evidence_path = self.cockroach / "evidence.json"
        evidence = json.loads(evidence_path.read_text(encoding="utf-8"))
        evidence["artifacts"].pop("evolution-plan.json")
        evidence_path.write_text(json.dumps(evidence) + "\n", encoding="utf-8")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_artifact_path_escape(self) -> None:
        outside = self.root / "outside.txt"
        outside.write_text("outside\n", encoding="utf-8")
        evidence_path = self.cockroach / "evidence.json"
        evidence = json.loads(evidence_path.read_text(encoding="utf-8"))
        evidence["artifacts"]["../outside.txt"] = digest(outside)
        evidence_path.write_text(json.dumps(evidence) + "\n", encoding="utf-8")
        self.assertEqual(self._run_compare(), 1)

    def test_rejects_invalid_routine_kind(self) -> None:
        plan_path = self.cockroach / "plan.json"
        plan = json.loads(plan_path.read_text(encoding="utf-8"))
        plan["changes"][4]["kind"] = "aggregate"
        plan_path.write_text(json.dumps(plan) + "\n", encoding="utf-8")
        self._refresh_digest(self.cockroach, "plan.json")
        self.assertEqual(self._run_compare(), 1)


if __name__ == "__main__":
    unittest.main()
