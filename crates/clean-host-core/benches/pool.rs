//! Instance-pool microbenchmarks (§1.8: "sub-millisecond instance checkout
//! from a warm pool after amortization").
//!
//! Checkout sits on every request, so its cost is multiplied by throughput.
//! The pool is measured directly rather than through a server, because a
//! network-bound measurement would hide a regression here entirely.

use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use clean_host_core::pool::{InstancePool, PoolConfig};
use clean_host_core::runtime::{Instance, LoadedComponent, RuntimeError};

/// A component whose instances cost nothing to create, so the measurement is
/// of the pool's own bookkeeping rather than of a Wasm engine.
struct NullComponent;

struct NullInstance;

impl Instance for NullInstance {
    fn reset(&mut self) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl LoadedComponent for NullComponent {
    fn instantiate(&self) -> Result<Box<dyn Instance>, RuntimeError> {
        Ok(Box::new(NullInstance))
    }
    fn imports(&self) -> Vec<String> {
        vec![]
    }
    fn exports(&self) -> Vec<String> {
        vec![]
    }
}

fn pool(min: u32, max: u32) -> Arc<InstancePool> {
    Arc::new(
        InstancePool::new(
            Arc::new(NullComponent),
            PoolConfig {
                instances_min: min,
                instances_max: max,
                instance_idle: std::time::Duration::from_secs(30),
                checkout_timeout: std::time::Duration::from_secs(5),
            },
            0,
        )
        .expect("pool builds"),
    )
}

fn bench(name: &str, iterations: u32, mut body: impl FnMut()) {
    for _ in 0..(iterations / 10).max(1) {
        body();
    }

    let started = Instant::now();
    for _ in 0..iterations {
        body();
    }
    let per_op = started.elapsed() / iterations;

    let target = if per_op.as_micros() < 1000 {
        "ok"
    } else {
        "OVER"
    };
    println!(
        "{name:<44} {:>9} ns/op   {:>8} vs 1ms target",
        per_op.as_nanos(),
        target
    );
}

fn main() {
    println!("clean-host-core pool microbenchmarks");
    println!("§1.8 target: sub-millisecond checkout from a warm pool\n");

    let warm = pool(64, 128);
    bench("checkout + return (warm pool)", 200_000, || {
        let guard = warm.checkout().expect("a warm pool always has one");
        black_box(&guard);
        // Dropping resets the instance and returns it — the full round trip a
        // request pays, not just the take.
        drop(guard);
    });

    // Depth one: every checkout takes the same slot, so this isolates the
    // lock and reset cost from any queueing effect.
    let single = pool(1, 1);
    bench("checkout + return (single instance)", 200_000, || {
        let guard = single.checkout().expect("available");
        black_box(&guard);
        drop(guard);
    });

    // Holding several at once is the shape of concurrent traffic.
    let concurrent = pool(8, 32);
    bench("8 concurrent checkouts", 50_000, || {
        let guards: Vec<_> = (0..8)
            .map(|_| concurrent.checkout().expect("under the ceiling"))
            .collect();
        black_box(&guards);
        drop(guards);
    });

    // Growth beyond instances-min instantiates, which is the expensive path.
    let growing = pool(1, 64);
    bench("checkout that grows the pool", 50_000, || {
        let guards: Vec<_> = (0..4)
            .map(|_| growing.checkout().expect("under the ceiling"))
            .collect();
        black_box(&guards);
        drop(guards);
    });

    bench("health snapshot", 200_000, || {
        black_box(warm.health());
    });

    println!();
    println!("Instance creation here is free — this measures the pool's own");
    println!("bookkeeping. Real checkout adds the runtime's instantiate cost");
    println!("when the pool has to grow.");
}
