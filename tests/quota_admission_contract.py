#!/usr/bin/env python3
"""Reference conformance model for the shared quota/admission contract.

This intentionally has no third-party dependencies. It tests semantics that
all production implementations must preserve; it is not a substitute for the
Redis/Valkey integration tests that should pin an exact production SHA.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, field
from email.utils import parsedate_to_datetime
from enum import Enum
from ipaddress import ip_address, ip_network
from random import Random
from threading import Lock, Thread
from typing import Mapping


class Kind(str, Enum):
    ALLOW = "allow"
    QUOTA = "quota_exhausted"
    CAPACITY = "capacity_exhausted"
    BACKEND = "quota_backend_unavailable"
    STALE_EPOCH = "stale_quota_epoch"
    FUTURE_EPOCH = "policy_epoch_unavailable"
    IDEMPOTENCY_CONFLICT = "idempotency_conflict"
    INVALID_OPERATION = "invalid_operation"


@dataclass(frozen=True)
class Policy:
    epoch: int = 10
    windows: Mapping[str, int] = field(
        default_factory=lambda: {"minute": 60, "ten-minute": 300, "hour": 1000}
    )
    concurrency: int = 4
    costs: Mapping[str, int] = field(
        default_factory=lambda: {"default": 1, "expensive": 5}
    )

    def validate(self) -> None:
        if not 1 <= len(self.windows) <= 8:
            raise ValueError("window count must be bounded")
        if self.concurrency < 1:
            raise ValueError("concurrency must be positive")
        if not 1 <= len(self.costs) <= 128:
            raise ValueError("operation table must be bounded")
        if any(limit < 1 for limit in self.windows.values()):
            raise ValueError("window limits must be positive")
        if any(cost < 1 or cost > 1_000_000 for cost in self.costs.values()):
            raise ValueError("operation costs must be positive and bounded")


@dataclass(frozen=True)
class Decision:
    kind: Kind
    status: int
    lease: str | None = None
    retry_after: int | None = None
    replayed: bool = False


class QuotaEngine:
    """Thread-safe semantic model of one quota partition."""

    def __init__(self, policy: Policy):
        policy.validate()
        self.policy = policy
        self.usage = {name: 0 for name in policy.windows}
        self.leases: set[str] = set()
        self.completed: dict[str, tuple[str, Decision]] = {}
        self.backend_available = True
        self._lease_seq = 0
        self._lock = Lock()

    def snapshot(self) -> tuple[dict[str, int], int, int]:
        with self._lock:
            return dict(self.usage), len(self.leases), len(self.completed)

    def admit(
        self,
        *,
        operation: str = "default",
        quota_epoch: int = 10,
        idempotency_key: str | None = None,
        fingerprint: str = "request",
        client_claimed_cost: int | None = None,
    ) -> Decision:
        del client_claimed_cost  # Client input must never choose the quota cost.
        with self._lock:
            if not self.backend_available:
                return Decision(Kind.BACKEND, 503, retry_after=1)

            if quota_epoch < self.policy.epoch:
                return Decision(Kind.STALE_EPOCH, 401)
            if quota_epoch > self.policy.epoch:
                return Decision(Kind.FUTURE_EPOCH, 503, retry_after=1)

            if operation not in self.policy.costs:
                return Decision(Kind.INVALID_OPERATION, 400)
            cost = self.policy.costs[operation]

            if idempotency_key:
                prior = self.completed.get(idempotency_key)
                if prior:
                    prior_fingerprint, prior_decision = prior
                    if prior_fingerprint != fingerprint:
                        return Decision(Kind.IDEMPOTENCY_CONFLICT, 409)
                    return Decision(
                        prior_decision.kind,
                        prior_decision.status,
                        lease=prior_decision.lease,
                        retry_after=prior_decision.retry_after,
                        replayed=True,
                    )

            # Capacity and every quota window are checked before any debit.
            if len(self.leases) >= self.policy.concurrency:
                return Decision(Kind.CAPACITY, 503, retry_after=1)

            for name, limit in self.policy.windows.items():
                if self.usage[name] + cost > limit:
                    return Decision(Kind.QUOTA, 429, retry_after=1)

            for name in self.usage:
                self.usage[name] += cost

            self._lease_seq += 1
            lease = f"lease-{self._lease_seq}"
            self.leases.add(lease)
            decision = Decision(Kind.ALLOW, 200, lease=lease)
            if idempotency_key:
                # Only accepted/final work is cached. Transient 429/503 responses
                # must be retryable after quota/capacity recovers.
                self.completed[idempotency_key] = (fingerprint, decision)
            return decision

    def release(self, lease: str) -> bool:
        with self._lock:
            if lease not in self.leases:
                return False
            self.leases.remove(lease)
            return True

    def expire(self, lease: str) -> bool:
        return self.release(lease)

    def reset_window_for_test(self, name: str) -> None:
        with self._lock:
            self.usage[name] = 0


class BoundedQueue:
    def __init__(self, limit: int):
        if limit < 1:
            raise ValueError("queue limit must be positive")
        self.limit = limit
        self._items: deque[tuple[str, float]] = deque()

    def enqueue(self, item: str, *, deadline: float) -> bool:
        if len(self._items) >= self.limit:
            return False
        self._items.append((item, deadline))
        return True

    def pop_ready(self, *, now: float) -> str | None:
        while self._items:
            item, deadline = self._items.popleft()
            if deadline >= now:
                return item
        return None

    def __len__(self) -> int:
        return len(self._items)


def _is_trusted(ip: str, trusted: tuple[str, ...]) -> bool:
    addr = ip_address(ip)
    return any(addr in ip_network(entry, strict=False) for entry in trusted)


def effective_client_ip(
    *,
    peer_ip: str,
    headers: Mapping[str, str],
    trusted_proxies: tuple[str, ...],
) -> str:
    """Ignore forwarding headers unless the immediate peer is trusted."""
    if not _is_trusted(peer_ip, trusted_proxies):
        return peer_ip

    cf_ip = headers.get("CF-Connecting-IP")
    if cf_ip:
        return str(ip_address(cf_ip.strip()))

    forwarded = [part.strip() for part in headers.get("X-Forwarded-For", "").split(",")]
    chain = [ip for ip in forwarded if ip] + [peer_ip]
    for candidate in reversed(chain):
        if not _is_trusted(candidate, trusted_proxies):
            return str(ip_address(candidate))
    return peer_ip


def retry_after_seconds(headers: Mapping[str, str], *, now_epoch: float) -> int | None:
    """Retry-After is authoritative when present; malformed values are ignored."""
    raw = headers.get("Retry-After")
    if raw is None:
        return None
    raw = raw.strip()
    if raw.isdigit():
        return max(0, int(raw))
    try:
        dt = parsedate_to_datetime(raw)
        return max(0, int(dt.timestamp() - now_epoch))
    except (TypeError, ValueError, OverflowError):
        return None


def test_multi_window_atomicity() -> None:
    q = QuotaEngine(
        Policy(
            windows={"minute": 2, "ten-minute": 3, "hour": 4},
            concurrency=1,
            costs={"default": 1, "expensive": 2},
        )
    )
    first = q.admit(idempotency_key="a")
    assert first.kind is Kind.ALLOW
    assert q.release(first.lease or "")

    second = q.admit(idempotency_key="b")
    assert second.kind is Kind.ALLOW
    assert q.release(second.lease or "")

    before = q.snapshot()
    denied = q.admit(idempotency_key="c")
    assert denied.kind is Kind.QUOTA and denied.status == 429
    assert q.snapshot() == before, "quota denial partially debited state"

    q.reset_window_for_test("minute")
    third = q.admit(idempotency_key="d")
    assert third.kind is Kind.ALLOW
    assert q.release(third.lease or "")

    before = q.snapshot()
    denied = q.admit(idempotency_key="e")
    assert denied.kind is Kind.QUOTA
    assert q.snapshot() == before


def test_idempotency_and_cost_integrity() -> None:
    q = QuotaEngine(
        Policy(
            windows={"minute": 10, "hour": 20},
            concurrency=2,
            costs={"default": 1, "expensive": 5},
        )
    )
    first = q.admit(
        operation="expensive",
        idempotency_key="same",
        fingerprint="payload-A",
        client_claimed_cost=0,
    )
    assert first.kind is Kind.ALLOW
    assert q.snapshot()[0] == {"minute": 5, "hour": 5}

    replay = q.admit(
        operation="expensive",
        idempotency_key="same",
        fingerprint="payload-A",
        client_claimed_cost=1,
    )
    assert replay.kind is Kind.ALLOW and replay.replayed
    assert q.snapshot()[0] == {"minute": 5, "hour": 5}

    conflict = q.admit(
        operation="default",
        idempotency_key="same",
        fingerprint="payload-B",
    )
    assert conflict.kind is Kind.IDEMPOTENCY_CONFLICT
    assert q.snapshot()[0] == {"minute": 5, "hour": 5}


def test_epoch_and_backend_failure_semantics() -> None:
    q = QuotaEngine(Policy(windows={"minute": 10}, concurrency=1))
    before = q.snapshot()
    assert q.admit(quota_epoch=9).kind is Kind.STALE_EPOCH
    assert q.snapshot() == before
    assert q.admit(quota_epoch=11).kind is Kind.FUTURE_EPOCH
    assert q.snapshot() == before

    q.backend_available = False
    failed = q.admit()
    assert failed.kind is Kind.BACKEND and failed.status == 503
    assert q.snapshot() == before


def test_capacity_and_fenced_release() -> None:
    q = QuotaEngine(Policy(windows={"minute": 100}, concurrency=1))
    first = q.admit(idempotency_key="a")
    assert first.kind is Kind.ALLOW
    before = q.snapshot()

    denied = q.admit(idempotency_key="b")
    assert denied.kind is Kind.CAPACITY and denied.status == 503
    assert q.snapshot() == before, "capacity denial consumed quota"

    old_lease = first.lease or ""
    assert q.expire(old_lease)
    second = q.admit(idempotency_key="b")
    assert second.kind is Kind.ALLOW
    assert not q.release(old_lease), "stale completion released a newer lease"
    assert q.snapshot()[1] == 1
    assert q.release(second.lease or "")


def test_high_contention_race() -> None:
    q = QuotaEngine(
        Policy(
            windows={"minute": 1000, "hour": 1000},
            concurrency=4,
            costs={"default": 1},
        )
    )
    results: list[Decision] = []
    result_lock = Lock()

    def hit(i: int) -> None:
        decision = q.admit(idempotency_key=f"k-{i}")
        with result_lock:
            results.append(decision)

    threads = [Thread(target=hit, args=(i,)) for i in range(100)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    assert sum(r.kind is Kind.ALLOW for r in results) == 4
    assert sum(r.kind is Kind.CAPACITY for r in results) == 96
    usage, active, _ = q.snapshot()
    assert usage == {"minute": 4, "hour": 4}
    assert active == 4


def test_deterministic_property_sequence() -> None:
    q = QuotaEngine(
        Policy(
            windows={"minute": 200, "ten-minute": 350, "hour": 500},
            concurrency=7,
            costs={"default": 1, "expensive": 3},
        )
    )
    rng = Random(20260929)
    live: list[str] = []

    for i in range(2000):
        if live and rng.random() < 0.42:
            lease = live.pop(rng.randrange(len(live)))
            assert q.release(lease)
            continue

        before_usage, before_active, _ = q.snapshot()
        decision = q.admit(
            operation="expensive" if rng.random() < 0.2 else "default",
            idempotency_key=f"prop-{i}",
        )
        after_usage, after_active, _ = q.snapshot()

        assert after_active <= q.policy.concurrency
        assert all(after_usage[name] <= limit for name, limit in q.policy.windows.items())
        if decision.kind is Kind.ALLOW:
            assert decision.lease is not None
            live.append(decision.lease)
        else:
            assert after_usage == before_usage
            assert after_active == before_active


def test_proxy_spoofing_defense() -> None:
    trusted = ("127.0.0.1/32", "10.0.0.0/8")
    spoofed = effective_client_ip(
        peer_ip="203.0.113.9",
        headers={"CF-Connecting-IP": "198.51.100.10", "X-Forwarded-For": "198.51.100.11"},
        trusted_proxies=trusted,
    )
    assert spoofed == "203.0.113.9"

    proxied = effective_client_ip(
        peer_ip="10.0.0.5",
        headers={"X-Forwarded-For": "198.51.100.7, 10.0.0.4"},
        trusted_proxies=trusted,
    )
    assert proxied == "198.51.100.7"


def test_backpressure_and_queue_bounds() -> None:
    assert retry_after_seconds({"Retry-After": "17"}, now_epoch=0) == 17
    assert retry_after_seconds({"Retry-After": "not-a-date"}, now_epoch=0) is None

    q = BoundedQueue(2)
    assert q.enqueue("expired", deadline=9)
    assert q.enqueue("live", deadline=20)
    assert not q.enqueue("overflow", deadline=30)
    assert q.pop_ready(now=10) == "live"
    assert q.pop_ready(now=10) is None


def main() -> None:
    tests = [
        test_multi_window_atomicity,
        test_idempotency_and_cost_integrity,
        test_epoch_and_backend_failure_semantics,
        test_capacity_and_fenced_release,
        test_high_contention_race,
        test_deterministic_property_sequence,
        test_proxy_spoofing_defense,
        test_backpressure_and_queue_bounds,
    ]
    for test in tests:
        test()
        print(f"PASS {test.__name__}")
    print(f"quota/admission conformance: PASS ({len(tests)} groups)")


if __name__ == "__main__":
    main()
