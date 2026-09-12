#!/usr/bin/env python3
"""Reject non-loopback or mismatched database URLs in the aggregate harness."""

from __future__ import annotations

import sys
from urllib.parse import urlsplit

ALLOWED_HOSTS = {"127.0.0.1", "::1", "localhost"}
ALLOWED_SCHEMES = {"postgres", "postgresql"}
ALLOWED_ADMIN_DATABASES = {"defaultdb", "postgres"}


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def parse_database_url(label: str, value: str) -> tuple[str, int | None, str]:
    parsed = urlsplit(value)
    require(parsed.scheme in ALLOWED_SCHEMES, f"{label} must use the PostgreSQL wire protocol")
    require(parsed.hostname is not None, f"{label} must include a hostname")
    hostname = parsed.hostname.lower()
    require(hostname in ALLOWED_HOSTS, f"{label} must use a loopback hostname")
    require(not parsed.fragment, f"{label} must not include a fragment")
    database = parsed.path.removeprefix("/")
    require(database != "" and "/" not in database, f"{label} must name exactly one database")
    return hostname, parsed.port, database


def validate(admin_url: str, target_url: str, database_name: str) -> None:
    admin_host, admin_port, admin_database = parse_database_url("admin URL", admin_url)
    target_host, target_port, target_database = parse_database_url("target URL", target_url)
    require(
        (admin_host, admin_port) == (target_host, target_port),
        "admin and target URLs must use the same loopback endpoint",
    )
    require(admin_database in ALLOWED_ADMIN_DATABASES, "admin URL must use postgres or defaultdb")
    require(target_database == database_name, "target URL database does not match the disposable database name")


def main() -> int:
    if len(sys.argv) != 4:
        print("usage: validate_ephemeral_urls.py ADMIN_URL TARGET_URL DATABASE_NAME", file=sys.stderr)
        return 2
    try:
        validate(*sys.argv[1:])
        print("ephemeral database URLs validated")
        return 0
    except (ValueError, TypeError) as exc:
        print(f"ephemeral database URL validation failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
