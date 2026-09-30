//! Durable, dependency-free conformance model for the shared quota/admission contract.
//!
//! This crate verifies semantic invariants that every runtime adapter must preserve.
//! It is deliberately not a Redis/Valkey implementation; production integration tests
//! should pin and exercise an exact production implementation SHA.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Allow,
    QuotaExhausted,
    CapacityExhausted,
    BackendUnavailable,
    StaleQuotaEpoch,
    FutureQuotaEpoch,
    IdempotencyConflict,
    InvalidOperation,
    InvalidRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub kind: Kind,
    pub status: u16,
    pub lease: Option<u64>,
    pub retry_after_secs: Option<u64>,
    pub replayed: bool,
}

impl Decision {
    fn new(kind: Kind, status: u16) -> Self {
        Self {
            kind,
            status,
            lease: None,
            retry_after_secs: None,
            replayed: false,
        }
    }

    fn retryable(kind: Kind, status: u16, retry_after_secs: u64) -> Self {
        Self {
            kind,
            status,
            lease: None,
            retry_after_secs: Some(retry_after_secs),
            replayed: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub epoch: u64,
    pub windows: BTreeMap<String, u64>,
    pub concurrency: usize,
    pub costs: BTreeMap<String, u64>,
    pub idempotency_capacity: usize,
}

impl Policy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=8).contains(&self.windows.len()) {
            return Err("window count must be between 1 and 8");
        }
        if self.windows.values().any(|limit| *limit == 0) {
            return Err("window limits must be positive");
        }
        if self.concurrency == 0 {
            return Err("concurrency must be positive");
        }
        if !(1..=128).contains(&self.costs.len()) {
            return Err("operation table must be between 1 and 128 entries");
        }
        if self
            .costs
            .values()
            .any(|cost| *cost == 0 || *cost > 1_000_000)
        {
            return Err("operation costs must be positive and bounded");
        }
        if !(1..=100_000).contains(&self.idempotency_capacity) {
            return Err("idempotency capacity must be bounded");
        }
        Ok(())
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            epoch: 10,
            windows: BTreeMap::from([
                ("minute".to_owned(), 60),
                ("ten-minute".to_owned(), 300),
                ("hour".to_owned(), 1000),
            ]),
            concurrency: 4,
            costs: BTreeMap::from([
                ("default".to_owned(), 1),
                ("expensive".to_owned(), 5),
            ]),
            idempotency_capacity: 1024,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    pub operation: &'a str,
    pub quota_epoch: u64,
    pub idempotency_key: Option<&'a str>,
    pub fingerprint: &'a str,
    /// Untrusted input included to prove clients cannot choose their own charge.
    pub client_claimed_cost: Option<u64>,
}

impl Default for Request<'_> {
    fn default() -> Self {
        Self {
            operation: "default",
            quota_epoch: 10,
            idempotency_key: None,
            fingerprint: "request",
            client_claimed_cost: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub usage: BTreeMap<String, u64>,
    pub active_leases: usize,
    pub idempotency_records: usize,
}

#[derive(Clone, Debug)]
struct Completed {
    fingerprint: String,
    decision: Decision,
}

#[derive(Debug)]
struct Inner {
    usage: BTreeMap<String, u64>,
    leases: HashSet<u64>,
    completed: HashMap<String, Completed>,
    completed_order: VecDeque<String>,
    backend_available: bool,
    lease_seq: u64,
}

#[derive(Debug)]
pub struct QuotaEngine {
    policy: Policy,
    inner: Mutex<Inner>,
}

impl QuotaEngine {
    pub fn new(policy: Policy) -> Result<Self, &'static str> {
        policy.validate()?;
        let usage = policy
            .windows
            .keys()
            .map(|name| (name.clone(), 0))
            .collect();

        Ok(Self {
            policy,
            inner: Mutex::new(Inner {
                usage,
                leases: HashSet::new(),
                completed: HashMap::new(),
                completed_order: VecDeque::new(),
                backend_available: true,
                lease_seq: 0,
            }),
        })
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().expect("quota mutex poisoned");
        Snapshot {
            usage: inner.usage.clone(),
            active_leases: inner.leases.len(),
            idempotency_records: inner.completed.len(),
        }
    }

    pub fn set_backend_available(&self, available: bool) {
        self.inner
            .lock()
            .expect("quota mutex poisoned")
            .backend_available = available;
    }

    pub fn admit(&self, request: Request<'_>) -> Decision {
        // The production hot path is stateful by definition. Keep every check and mutation
        // under one critical section in the model so a rejection can never partially debit.
        // HOT-PATH (imperative by design): models one atomic backend transaction.
        let mut inner = self.inner.lock().expect("quota mutex poisoned");

        if !inner.backend_available {
            return Decision::retryable(Kind::BackendUnavailable, 503, 1);
        }

        if request.quota_epoch < self.policy.epoch {
            return Decision::new(Kind::StaleQuotaEpoch, 401);
        }
        if request.quota_epoch > self.policy.epoch {
            return Decision::retryable(Kind::FutureQuotaEpoch, 503, 1);
        }

        if request.fingerprint.is_empty() || request.fingerprint.len() > 256 {
            return Decision::new(Kind::InvalidRequest, 400);
        }
        if let Some(key) = request.idempotency_key {
            if key.is_empty() || key.len() > 128 {
                return Decision::new(Kind::InvalidRequest, 400);
            }
        }

        let Some(cost) = self.policy.costs.get(request.operation).copied() else {
            return Decision::new(Kind::InvalidOperation, 400);
        };

        // Never use a caller-provided cost for enforcement.
        let _ignored_untrusted_cost = request.client_claimed_cost;

        if let Some(key) = request.idempotency_key {
            if let Some(prior) = inner.completed.get(key).cloned() {
                if prior.fingerprint != request.fingerprint {
                    return Decision::new(Kind::IdempotencyConflict, 409);
                }
                let mut replay = prior.decision;
                replay.replayed = true;
                return replay;
            }
        }

        if inner.leases.len() >= self.policy.concurrency {
            return Decision::retryable(Kind::CapacityExhausted, 503, 1);
        }

        for (name, limit) in &self.policy.windows {
            let used = inner.usage.get(name).copied().unwrap_or_default();
            if used.saturating_add(cost) > *limit {
                return Decision::retryable(Kind::QuotaExhausted, 429, 1);
            }
        }

        for used in inner.usage.values_mut() {
            *used += cost;
        }

        inner.lease_seq = inner.lease_seq.saturating_add(1);
        let lease = inner.lease_seq;
        inner.leases.insert(lease);

        let decision = Decision {
            kind: Kind::Allow,
            status: 200,
            lease: Some(lease),
            retry_after_secs: None,
            replayed: false,
        };

        if let Some(key) = request.idempotency_key {
            while inner.completed.len() >= self.policy.idempotency_capacity {
                if let Some(oldest) = inner.completed_order.pop_front() {
                    inner.completed.remove(&oldest);
                } else {
                    break;
                }
            }
            let key = key.to_owned();
            inner.completed_order.push_back(key.clone());
            inner.completed.insert(
                key,
                Completed {
                    fingerprint: request.fingerprint.to_owned(),
                    decision: decision.clone(),
                },
            );
        }

        decision
    }

    pub fn release(&self, lease: u64) -> bool {
        // HOT-PATH (imperative by design): compare-and-delete lease semantics.
        self.inner
            .lock()
            .expect("quota mutex poisoned")
            .leases
            .remove(&lease)
    }

    pub fn expire(&self, lease: u64) -> bool {
        self.release(lease)
    }

    pub fn reset_window_for_test(&self, name: &str) {
        let mut inner = self.inner.lock().expect("quota mutex poisoned");
        let used = inner
            .usage
            .get_mut(name)
            .unwrap_or_else(|| panic!("unknown window: {name}"));
        *used = 0;
    }
}

#[derive(Debug)]
pub struct BoundedQueue {
    limit: usize,
    items: VecDeque<(String, u64)>,
}

impl BoundedQueue {
    pub fn new(limit: usize) -> Result<Self, &'static str> {
        if limit == 0 {
            return Err("queue limit must be positive");
        }
        Ok(Self {
            limit,
            items: VecDeque::new(),
        })
    }

    pub fn enqueue(&mut self, item: impl Into<String>, deadline: u64) -> bool {
        // HOT-PATH (imperative by design): VecDeque gives O(1) push/pop semantics.
        if self.items.len() >= self.limit {
            return false;
        }
        self.items.push_back((item.into(), deadline));
        true
    }

    pub fn pop_ready(&mut self, now: u64) -> Option<String> {
        while let Some((item, deadline)) = self.items.pop_front() {
            if deadline >= now {
                return Some(item);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub fn effective_client_ip(
    peer_ip: IpAddr,
    cf_connecting_ip: Option<&str>,
    x_forwarded_for: Option<&str>,
    trusted_proxies: &HashSet<IpAddr>,
) -> IpAddr {
    if !trusted_proxies.contains(&peer_ip) {
        return peer_ip;
    }

    if let Some(candidate) = cf_connecting_ip.and_then(|raw| raw.trim().parse().ok()) {
        return candidate;
    }

    let mut chain = x_forwarded_for
        .into_iter()
        .flat_map(|raw| raw.split(','))
        .filter_map(|raw| raw.trim().parse::<IpAddr>().ok())
        .collect::<Vec<_>>();
    chain.push(peer_ip);

    chain
        .into_iter()
        .rev()
        .find(|candidate| !trusted_proxies.contains(candidate))
        .unwrap_or(peer_ip)
}

pub fn retry_delay_secs(
    retry_after: Option<&str>,
    rate_limit_hint_secs: Option<u64>,
) -> Option<u64> {
    retry_after
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .or(rate_limit_hint_secs)
}

pub fn rate_limit_headers(
    policy_id: &str,
    quota: u64,
    window_secs: u64,
    remaining: u64,
    reset_secs: u64,
) -> (String, String) {
    assert!(
        policy_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
        "policy id must be a safe token before serialization"
    );
    (
        format!("\"{policy_id}\";q={quota};w={window_secs}"),
        format!("\"{policy_id}\";r={remaining};t={reset_secs}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn policy(
        windows: &[(&str, u64)],
        concurrency: usize,
        costs: &[(&str, u64)],
    ) -> Policy {
        Policy {
            epoch: 10,
            windows: windows
                .iter()
                .map(|(name, limit)| ((*name).to_owned(), *limit))
                .collect(),
            concurrency,
            costs: costs
                .iter()
                .map(|(name, cost)| ((*name).to_owned(), *cost))
                .collect(),
            idempotency_capacity: 1024,
        }
    }

    fn request<'a>(key: Option<&'a str>) -> Request<'a> {
        Request {
            idempotency_key: key,
            ..Request::default()
        }
    }

    #[test]
    fn multi_window_denials_are_atomic() {
        let engine = QuotaEngine::new(policy(
            &[("minute", 2), ("ten-minute", 3), ("hour", 4)],
            1,
            &[("default", 1), ("expensive", 2)],
        ))
        .unwrap();

        for key in ["a", "b"] {
            let allowed = engine.admit(request(Some(key)));
            assert_eq!(allowed.kind, Kind::Allow);
            assert!(engine.release(allowed.lease.unwrap()));
        }

        let before = engine.snapshot();
        let denied = engine.admit(request(Some("c")));
        assert_eq!(denied.kind, Kind::QuotaExhausted);
        assert_eq!(denied.status, 429);
        assert_eq!(engine.snapshot(), before);

        engine.reset_window_for_test("minute");
        let third = engine.admit(request(Some("d")));
        assert_eq!(third.kind, Kind::Allow);
        assert!(engine.release(third.lease.unwrap()));

        engine.reset_window_for_test("minute");
        let before = engine.snapshot();
        assert_eq!(
            engine.admit(request(Some("e"))).kind,
            Kind::QuotaExhausted
        );
        assert_eq!(engine.snapshot(), before);

        engine.reset_window_for_test("ten-minute");
        let fourth = engine.admit(request(Some("f")));
        assert_eq!(fourth.kind, Kind::Allow);
        assert!(engine.release(fourth.lease.unwrap()));

        engine.reset_window_for_test("minute");
        engine.reset_window_for_test("ten-minute");
        let before = engine.snapshot();
        assert_eq!(
            engine.admit(request(Some("g"))).kind,
            Kind::QuotaExhausted
        );
        assert_eq!(engine.snapshot(), before);
    }

    #[test]
    fn idempotency_is_bounded_and_client_cost_is_ignored() {
        let mut p = policy(
            &[("minute", 20), ("hour", 30)],
            2,
            &[("default", 1), ("expensive", 5)],
        );
        p.idempotency_capacity = 2;
        let engine = QuotaEngine::new(p).unwrap();

        let first = engine.admit(Request {
            operation: "expensive",
            idempotency_key: Some("same"),
            fingerprint: "payload-A",
            client_claimed_cost: Some(0),
            ..Request::default()
        });
        assert_eq!(first.kind, Kind::Allow);
        assert_eq!(
            engine.snapshot().usage,
            BTreeMap::from([("hour".to_owned(), 5), ("minute".to_owned(), 5)])
        );

        let replay = engine.admit(Request {
            operation: "expensive",
            idempotency_key: Some("same"),
            fingerprint: "payload-A",
            client_claimed_cost: Some(1),
            ..Request::default()
        });
        assert_eq!(replay.kind, Kind::Allow);
        assert!(replay.replayed);
        assert_eq!(
            engine.snapshot().usage,
            BTreeMap::from([("hour".to_owned(), 5), ("minute".to_owned(), 5)])
        );

        let conflict = engine.admit(Request {
            idempotency_key: Some("same"),
            fingerprint: "payload-B",
            ..Request::default()
        });
        assert_eq!(conflict.kind, Kind::IdempotencyConflict);

        assert!(engine.release(first.lease.unwrap()));
        for key in ["two", "three"] {
            let allowed = engine.admit(request(Some(key)));
            assert_eq!(allowed.kind, Kind::Allow);
            assert!(engine.release(allowed.lease.unwrap()));
        }
        assert_eq!(engine.snapshot().idempotency_records, 2);
    }

    #[test]
    fn epoch_and_backend_failures_do_not_debit() {
        let engine = QuotaEngine::new(policy(
            &[("minute", 10)],
            1,
            &[("default", 1)],
        ))
        .unwrap();
        let before = engine.snapshot();

        let stale = engine.admit(Request {
            quota_epoch: 9,
            ..Request::default()
        });
        assert_eq!(stale.kind, Kind::StaleQuotaEpoch);
        assert_eq!(stale.status, 401);
        assert_eq!(engine.snapshot(), before);

        let future = engine.admit(Request {
            quota_epoch: 11,
            ..Request::default()
        });
        assert_eq!(future.kind, Kind::FutureQuotaEpoch);
        assert_eq!(future.status, 503);
        assert_eq!(engine.snapshot(), before);

        engine.set_backend_available(false);
        let unavailable = engine.admit(Request::default());
        assert_eq!(unavailable.kind, Kind::BackendUnavailable);
        assert_eq!(unavailable.status, 503);
        assert_eq!(engine.snapshot(), before);
    }

    #[test]
    fn capacity_is_distinct_and_release_is_fenced() {
        let engine = QuotaEngine::new(policy(
            &[("minute", 100)],
            1,
            &[("default", 1)],
        ))
        .unwrap();

        let first = engine.admit(request(Some("a")));
        assert_eq!(first.kind, Kind::Allow);
        let before = engine.snapshot();

        let transient = engine.admit(request(Some("b")));
        assert_eq!(transient.kind, Kind::CapacityExhausted);
        assert_eq!(transient.status, 503);
        assert_eq!(engine.snapshot(), before);

        let old_lease = first.lease.unwrap();
        assert!(engine.expire(old_lease));

        let second = engine.admit(request(Some("b")));
        assert_eq!(second.kind, Kind::Allow);
        assert!(!engine.release(old_lease));
        assert_eq!(engine.snapshot().active_leases, 1);
        assert!(engine.release(second.lease.unwrap()));
    }

    #[test]
    fn high_contention_cannot_over_admit() {
        let engine = Arc::new(
            QuotaEngine::new(policy(
                &[("minute", 1000), ("hour", 1000)],
                4,
                &[("default", 1)],
            ))
            .unwrap(),
        );
        let results = Arc::new(Mutex::new(Vec::<Decision>::new()));

        let threads = (0..100)
            .map(|i| {
                let engine = Arc::clone(&engine);
                let results = Arc::clone(&results);
                thread::spawn(move || {
                    let key = format!("k-{i}");
                    let result = engine.admit(Request {
                        idempotency_key: Some(&key),
                        ..Request::default()
                    });
                    results.lock().unwrap().push(result);
                })
            })
            .collect::<Vec<_>>();

        for thread in threads {
            thread.join().unwrap();
        }

        let results = results.lock().unwrap();
        assert_eq!(
            results.iter().filter(|r| r.kind == Kind::Allow).count(),
            4
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| r.kind == Kind::CapacityExhausted)
                .count(),
            96
        );
        let snapshot = engine.snapshot();
        assert_eq!(snapshot.active_leases, 4);
        assert_eq!(
            snapshot.usage,
            BTreeMap::from([
                ("hour".to_owned(), 4),
                ("minute".to_owned(), 4),
            ])
        );
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }

        fn index(&mut self, len: usize) -> usize {
            (self.next() as usize) % len
        }
    }

    #[test]
    fn deterministic_property_sequence_preserves_invariants() {
        let engine = QuotaEngine::new(policy(
            &[("minute", 200), ("ten-minute", 350), ("hour", 500)],
            7,
            &[("default", 1), ("expensive", 3)],
        ))
        .unwrap();
        let mut rng = Lcg(20260929);
        let mut live = Vec::<u64>::new();

        for i in 0..2000 {
            if !live.is_empty() && rng.next() % 100 < 42 {
                let index = rng.index(live.len());
                let lease = live.swap_remove(index);
                assert!(engine.release(lease));
                continue;
            }

            let before = engine.snapshot();
            let key = format!("prop-{i}");
            let result = engine.admit(Request {
                operation: if rng.next() % 5 == 0 {
                    "expensive"
                } else {
                    "default"
                },
                idempotency_key: Some(&key),
                ..Request::default()
            });
            let after = engine.snapshot();

            assert!(after.active_leases <= engine.policy().concurrency);
            for (name, limit) in &engine.policy().windows {
                assert!(after.usage[name] <= *limit);
            }

            if result.kind == Kind::Allow {
                live.push(result.lease.unwrap());
            } else {
                assert_eq!(after.usage, before.usage);
                assert_eq!(after.active_leases, before.active_leases);
            }
        }
    }

    #[test]
    fn direct_callers_cannot_spoof_forwarded_ip() {
        let trusted = HashSet::from([
            "127.0.0.1".parse::<IpAddr>().unwrap(),
            "10.0.0.5".parse::<IpAddr>().unwrap(),
            "10.0.0.4".parse::<IpAddr>().unwrap(),
        ]);

        let direct = effective_client_ip(
            "203.0.113.9".parse().unwrap(),
            Some("198.51.100.10"),
            Some("198.51.100.11"),
            &trusted,
        );
        assert_eq!(direct, "203.0.113.9".parse::<IpAddr>().unwrap());

        let proxied = effective_client_ip(
            "10.0.0.5".parse().unwrap(),
            None,
            Some("198.51.100.7, 10.0.0.4"),
            &trusted,
        );
        assert_eq!(proxied, "198.51.100.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn backpressure_is_bounded_and_retry_after_wins() {
        assert_eq!(retry_delay_secs(Some("17"), Some(999)), Some(17));
        assert_eq!(retry_delay_secs(Some("not-a-number"), Some(9)), Some(9));
        assert_eq!(retry_delay_secs(None, None), None);

        let mut queue = BoundedQueue::new(2).unwrap();
        assert!(queue.enqueue("expired", 9));
        assert!(queue.enqueue("live", 20));
        assert!(!queue.enqueue("overflow", 30));
        assert_eq!(queue.pop_ready(10).as_deref(), Some("live"));
        assert!(queue.pop_ready(10).is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn policy_and_header_serialization_are_bounded() {
        let invalid = Policy {
            windows: BTreeMap::new(),
            ..Policy::default()
        };
        assert!(QuotaEngine::new(invalid).is_err());

        let invalid = Policy {
            costs: BTreeMap::from([("bad".to_owned(), 0)]),
            ..Policy::default()
        };
        assert!(QuotaEngine::new(invalid).is_err());

        let (policy, state) = rate_limit_headers("free-v3", 60, 60, 12, 18);
        assert_eq!(policy, "\"free-v3\";q=60;w=60");
        assert_eq!(state, "\"free-v3\";r=12;t=18");
        assert!(!policy.contains("tenant"));
        assert!(!state.contains("tenant"));
    }
}
