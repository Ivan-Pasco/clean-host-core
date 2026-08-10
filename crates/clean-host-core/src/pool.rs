//! Instance pooling (CLNH-27..31).
//!
//! Instance count, not thread count, is the concurrency ceiling: a host with
//! `instances-max = 128` serves 128 concurrent invocations before checkout
//! waits.
//!
//! The pool is deliberately sync (`Mutex` + `Condvar`) rather than async. It is
//! called from the concrete host's async request path, but a checkout either
//! takes an idle instance immediately or blocks a bounded time; making it async
//! would pull an executor dependency into a library that CH-01 forbids from
//! owning any I/O.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::runtime::{Instance, LoadedComponent, RuntimeError};

/// Live pool counters, surfaced through [`crate::HealthReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolHealth {
    pub instances_min: u32,
    pub instances_max: u32,
    pub instances_current: u32,
    pub checkouts_active: u32,
    pub checkouts_queued: u32,
}

/// How the pool is sized and how long a checkout may wait.
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    pub instances_min: u32,
    pub instances_max: u32,
    pub instance_idle: Duration,
    /// How long `checkout` blocks before giving up (CLNH-30). The concrete host
    /// decides what to do with the failure — clean-server returns 503.
    pub checkout_timeout: Duration,
}

struct Idle {
    instance: Box<dyn Instance>,
    since: Instant,
}

struct Inner {
    idle: VecDeque<Idle>,
    /// Instances that exist: idle + checked out.
    live: u32,
    active: u32,
    queued: u32,
    /// Set on shutdown; makes every subsequent checkout fail fast (CLNH-57).
    closed: bool,
    /// Bumped on every generation swap so guards from a superseded component
    /// are dropped rather than returned to the pool (CLNH-52).
    generation: u64,
}

/// A pool of ready-to-use instances of one composed component.
pub struct InstancePool {
    inner: Mutex<Inner>,
    available: Condvar,
    config: PoolConfig,
    component: Arc<dyn LoadedComponent>,
    /// Identifies the composed-component generation this pool belongs to. A
    /// guard minted before a reload carries the old value and is discarded on
    /// return rather than pooled (CLNH-52).
    generation: u64,
}

impl InstancePool {
    /// Build a pool and eagerly fill it to `instances_min` (CLNH-27).
    ///
    /// Pre-filling is what buys the sub-millisecond checkout target: the first
    /// request must not pay instantiation cost.
    pub fn new(
        component: Arc<dyn LoadedComponent>,
        config: PoolConfig,
        generation: u64,
    ) -> Result<Self, RuntimeError> {
        let mut idle = VecDeque::new();
        for _ in 0..config.instances_min {
            idle.push_back(Idle {
                instance: component.instantiate()?,
                since: Instant::now(),
            });
        }
        let live = idle.len() as u32;

        Ok(Self {
            inner: Mutex::new(Inner {
                idle,
                live,
                active: 0,
                queued: 0,
                closed: false,
                generation,
            }),
            available: Condvar::new(),
            config,
            component,
            generation,
        })
    }

    /// Take one instance for a single invocation.
    ///
    /// Returns an idle instance if there is one; grows the pool up to
    /// `instances_max`; otherwise waits up to `checkout_timeout` (CLNH-30).
    pub fn checkout(self: &Arc<Self>) -> Result<InstanceGuard, PoolError> {
        let deadline = Instant::now() + self.config.checkout_timeout;
        let mut inner = self.inner.lock().unwrap();

        loop {
            if inner.closed {
                return Err(PoolError::Closed);
            }

            if let Some(idle) = inner.idle.pop_front() {
                inner.active += 1;
                return Ok(self.guard(idle.instance));
            }

            if inner.live < self.config.instances_max {
                // Grow rather than wait. Instantiating under the lock keeps
                // `live` honest; the alternative (release, instantiate,
                // re-acquire) lets concurrent callers race past the ceiling.
                inner.live += 1;
                match self.component.instantiate() {
                    Ok(instance) => {
                        inner.active += 1;
                        return Ok(self.guard(instance));
                    }
                    Err(e) => {
                        inner.live -= 1;
                        return Err(PoolError::Runtime(e));
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PoolError::Exhausted {
                    waited: self.config.checkout_timeout,
                });
            }

            inner.queued += 1;
            let (next, timeout) = self.available.wait_timeout(inner, remaining).unwrap();
            inner = next;
            inner.queued -= 1;

            if timeout.timed_out() && inner.idle.is_empty() {
                return Err(PoolError::Exhausted {
                    waited: self.config.checkout_timeout,
                });
            }
        }
    }

    fn guard(self: &Arc<Self>, instance: Box<dyn Instance>) -> InstanceGuard {
        InstanceGuard {
            instance: Some(instance),
            pool: Arc::clone(self),
            generation: self.generation,
        }
    }

    /// Return an instance after use: reset it and make it available again
    /// (CLNH-29).
    fn give_back(&self, mut instance: Box<dyn Instance>, generation: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.active = inner.active.saturating_sub(1);

        // A guard from a superseded generation is dropped, not pooled — its
        // instance belongs to a composed component that is on its way out.
        if generation != inner.generation || inner.closed {
            inner.live = inner.live.saturating_sub(1);
            drop(inner);
            self.available.notify_one();
            return;
        }

        match instance.reset() {
            Ok(()) => {
                inner.idle.push_back(Idle {
                    instance,
                    since: Instant::now(),
                });
            }
            Err(_) => {
                // A reset failure means the instance cannot be trusted for the
                // next request. Drop it; the pool refills on demand. Silently
                // reusing it would leak state across requests.
                inner.live = inner.live.saturating_sub(1);
            }
        }
        drop(inner);
        self.available.notify_one();
    }

    /// Drop idle instances above `instances_min` that have been idle longer
    /// than `instance_idle` (CLNH-31). The concrete host calls this on a timer.
    pub fn reap_idle(&self) -> u32 {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let mut reaped = 0;

        while inner.live > self.config.instances_min {
            let stale = inner
                .idle
                .front()
                .is_some_and(|i| now.duration_since(i.since) >= self.config.instance_idle);
            if !stale {
                break;
            }
            inner.idle.pop_front();
            inner.live -= 1;
            reaped += 1;
        }
        reaped
    }

    pub fn health(&self) -> PoolHealth {
        let inner = self.inner.lock().unwrap();
        PoolHealth {
            instances_min: self.config.instances_min,
            instances_max: self.config.instances_max,
            instances_current: inner.live,
            checkouts_active: inner.active,
            checkouts_queued: inner.queued,
        }
    }

    /// Stop handing out instances and wake every waiter (CLNH-57).
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.closed = true;
        inner.idle.clear();
        drop(inner);
        self.available.notify_all();
    }

    /// Number of guards still outstanding. Shutdown drains on this (CLNH-58).
    pub fn active(&self) -> u32 {
        self.inner.lock().unwrap().active
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("pool exhausted: no instance available after {waited:?}")]
    Exhausted { waited: Duration },
    #[error("host is shutting down")]
    Closed,
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

/// RAII handle to one checked-out instance. Dropping it resets the instance and
/// returns it to the pool (CLNH-28/29).
///
/// Holds an `Arc` to the pool rather than a borrow, so the guard is `'static`
/// and can be moved into the concrete host's async request task.
pub struct InstanceGuard {
    instance: Option<Box<dyn Instance>>,
    pool: Arc<InstancePool>,
    generation: u64,
}

impl std::fmt::Debug for InstanceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceGuard")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl InstanceGuard {
    pub fn instance(&mut self) -> &mut dyn Instance {
        self.instance
            .as_mut()
            .expect("instance taken only on drop")
            .as_mut()
    }

    /// Downcast to the adapter's concrete instance type so the concrete host
    /// can invoke typed exports.
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.instance().as_any_mut().downcast_mut::<T>()
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(instance) = self.instance.take() {
            self.pool.give_back(instance, self.generation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct FakeInstance {
        resets: Arc<AtomicU32>,
        reset_fails: bool,
    }

    impl Instance for FakeInstance {
        fn reset(&mut self) -> Result<(), RuntimeError> {
            self.resets.fetch_add(1, Ordering::SeqCst);
            if self.reset_fails {
                Err(RuntimeError::Reset("forced".into()))
            } else {
                Ok(())
            }
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    struct FakeComponent {
        made: Arc<AtomicU32>,
        resets: Arc<AtomicU32>,
        reset_fails: bool,
    }

    impl LoadedComponent for FakeComponent {
        fn instantiate(&self) -> Result<Box<dyn Instance>, RuntimeError> {
            self.made.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeInstance {
                resets: Arc::clone(&self.resets),
                reset_fails: self.reset_fails,
            }))
        }
        fn imports(&self) -> Vec<String> {
            vec![]
        }
        fn exports(&self) -> Vec<String> {
            vec![]
        }
    }

    fn pool(min: u32, max: u32) -> (Arc<InstancePool>, Arc<AtomicU32>, Arc<AtomicU32>) {
        pool_with(min, max, false, Duration::from_millis(50))
    }

    fn pool_with(
        min: u32,
        max: u32,
        reset_fails: bool,
        timeout: Duration,
    ) -> (Arc<InstancePool>, Arc<AtomicU32>, Arc<AtomicU32>) {
        let made = Arc::new(AtomicU32::new(0));
        let resets = Arc::new(AtomicU32::new(0));
        let component = Arc::new(FakeComponent {
            made: Arc::clone(&made),
            resets: Arc::clone(&resets),
            reset_fails,
        });
        let p = InstancePool::new(
            component,
            PoolConfig {
                instances_min: min,
                instances_max: max,
                instance_idle: Duration::from_millis(10),
                checkout_timeout: timeout,
            },
            0,
        )
        .unwrap();
        (Arc::new(p), made, resets)
    }

    #[test]
    fn prefills_to_instances_min() {
        let (p, made, _) = pool(4, 8);
        assert_eq!(made.load(Ordering::SeqCst), 4);
        assert_eq!(p.health().instances_current, 4);
    }

    #[test]
    fn guard_returns_and_resets_the_instance() {
        let (p, _, resets) = pool(1, 4);
        {
            let _g = p.checkout().unwrap();
            assert_eq!(p.health().checkouts_active, 1);
        }
        assert_eq!(resets.load(Ordering::SeqCst), 1);
        assert_eq!(p.health().checkouts_active, 0);
        assert_eq!(p.health().instances_current, 1);
    }

    #[test]
    fn grows_up_to_max_then_reports_exhaustion() {
        let (p, made, _) = pool(1, 2);
        let _a = p.checkout().unwrap();
        let _b = p.checkout().unwrap();
        assert_eq!(made.load(Ordering::SeqCst), 2);

        // CLNH-30: third checkout waits, then fails rather than exceeding max.
        let err = p.checkout().unwrap_err();
        assert!(matches!(err, PoolError::Exhausted { .. }), "{err:?}");
        assert_eq!(p.health().instances_current, 2);
    }

    #[test]
    fn a_returned_instance_unblocks_a_waiter() {
        let (p, _, _) = pool_with(1, 1, false, Duration::from_secs(2));
        let guard = p.checkout().unwrap();

        let p2 = Arc::clone(&p);
        let waiter = std::thread::spawn(move || p2.checkout().map(|_| ()));

        std::thread::sleep(Duration::from_millis(50));
        drop(guard);

        assert!(
            waiter.join().unwrap().is_ok(),
            "waiter should get the instance"
        );
    }

    #[test]
    fn an_instance_that_fails_reset_is_dropped_not_reused() {
        let (p, _, _) = pool_with(1, 4, true, Duration::from_millis(50));
        {
            let _g = p.checkout().unwrap();
        }
        // Reset failed, so the instance must not go back into the idle set.
        assert_eq!(p.health().instances_current, 0);
    }

    #[test]
    fn reap_idle_keeps_instances_min() {
        let (p, _, _) = pool(2, 8);
        // Force growth to 4, then return everything.
        {
            let _a = p.checkout().unwrap();
            let _b = p.checkout().unwrap();
            let _c = p.checkout().unwrap();
            let _d = p.checkout().unwrap();
        }
        assert_eq!(p.health().instances_current, 4);

        std::thread::sleep(Duration::from_millis(20));
        let reaped = p.reap_idle();
        assert_eq!(reaped, 2);
        assert_eq!(p.health().instances_current, 2);
    }

    #[test]
    fn close_rejects_further_checkouts() {
        let (p, _, _) = pool(1, 4);
        p.close();
        assert!(matches!(p.checkout().unwrap_err(), PoolError::Closed));
    }
}
