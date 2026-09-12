from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "scripts" / "validate_ephemeral_urls.py"
SPEC = importlib.util.spec_from_file_location("validate_ephemeral_urls", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
VALIDATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATE)


class ValidateEphemeralUrlsTests(unittest.TestCase):
    def test_accepts_postgres_loopback_pair(self) -> None:
        VALIDATE.validate(
            "postgres://postgres:postgres@127.0.0.1:5432/postgres",
            "postgres://postgres:postgres@127.0.0.1:5432/declmig_target",
            "declmig_target",
        )

    def test_accepts_cockroach_loopback_pair(self) -> None:
        VALIDATE.validate(
            "postgresql://root@localhost:26257/defaultdb?sslmode=disable",
            "postgresql://root@localhost:26257/declmig_target?sslmode=disable",
            "declmig_target",
        )

    def test_rejects_remote_target(self) -> None:
        with self.assertRaisesRegex(ValueError, "loopback"):
            VALIDATE.validate(
                "postgres://operator@db.example.test/postgres",
                "postgres://operator@db.example.test/declmig_target",
                "declmig_target",
            )

    def test_rejects_mismatched_endpoint(self) -> None:
        with self.assertRaisesRegex(ValueError, "same loopback endpoint"):
            VALIDATE.validate(
                "postgres://postgres@127.0.0.1:5432/postgres",
                "postgres://postgres@127.0.0.1:55432/declmig_target",
                "declmig_target",
            )

    def test_rejects_wrong_target_database(self) -> None:
        with self.assertRaisesRegex(ValueError, "disposable database name"):
            VALIDATE.validate(
                "postgresql://root@127.0.0.1:26257/defaultdb?sslmode=disable",
                "postgresql://root@127.0.0.1:26257/application?sslmode=disable",
                "declmig_target",
            )


if __name__ == "__main__":
    unittest.main()
