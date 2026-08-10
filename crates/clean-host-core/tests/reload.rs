//! Reload semantics (§10, CLNH-49..54).
//!
//! Reload is the one operation that can leave a host in a broken state if it
//! goes wrong halfway, so the failure paths matter more than the happy one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use clean_host_core::runtime::{
    Instance, LoadedComponent, RuntimeCapabilities, RuntimeError, WasmRuntime,
};
use clean_host_core::{Host, HostConfig, HostProvided};

/// A runtime whose loads can be made to fail on demand.
struct FakeRuntime {
    loads: Arc<AtomicU32>,
    fail_after: Arc<AtomicU32>,
}

impl FakeRuntime {
    fn new() -> (Self, Arc<AtomicU32>, Arc<AtomicU32>) {
        let loads = Arc::new(AtomicU32::new(0));
        // u32::MAX means "never fail".
        let fail_after = Arc::new(AtomicU32::new(u32::MAX));
        (
            Self {
                loads: Arc::clone(&loads),
                fail_after: Arc::clone(&fail_after),
            },
            loads,
            fail_after,
        )
    }
}

struct FakeComponent {
    generation: u32,
}

struct FakeInstance {
    generation: u32,
}

impl Instance for FakeInstance {
    fn reset(&mut self) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl LoadedComponent for FakeComponent {
    fn instantiate(&self) -> Result<Box<dyn Instance>, RuntimeError> {
        Ok(Box::new(FakeInstance {
            generation: self.generation,
        }))
    }
    fn imports(&self) -> Vec<String> {
        vec![]
    }
    fn exports(&self) -> Vec<String> {
        vec!["handle".to_string()]
    }
}

impl WasmRuntime for FakeRuntime {
    fn load(&self, _bytes: &[u8]) -> Result<Box<dyn LoadedComponent>, RuntimeError> {
        let n = self.loads.fetch_add(1, Ordering::SeqCst);
        if n >= self.fail_after.load(Ordering::SeqCst) {
            return Err(RuntimeError::Load("forced failure".into()));
        }
        Ok(Box::new(FakeComponent { generation: n }))
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            epoch_interruption: true,
            pooling: true,
        }
    }

    fn engine_id(&self) -> String {
        "fake".into()
    }

    fn introspector(&self) -> Box<clean_host_core::bridge::Introspect> {
        Box::new(|_| Ok((vec![], vec!["handle".to_string()])))
    }
}

/// A config pointing at a guest file that exists (contents are never parsed by
/// the fake runtime).
fn host_with(dir: &tempfile::TempDir) -> (Host, Arc<AtomicU32>, Arc<AtomicU32>) {
    let guest = dir.path().join("app.wasm");
    std::fs::write(&guest, b"\0asm\x0d\0\x01\0").unwrap();

    let config = HostConfig::parse(
        &format!(
            r#"
[host]
name = "clean-server"
version = "0.1.0"
component-model = "0.3.0"

[guest]
name = "app"
wasm = "{}"
world = "clean:host/server@0.1"

[runtime]
instances-min = 1
instances-max = 4
"#,
            guest.display()
        ),
        dir.path().join("host.toml"),
    )
    .unwrap();

    let (runtime, loads, fail_after) = FakeRuntime::new();
    let mut host = Host::new(config, Box::new(runtime)).unwrap();
    host.set_host_provided(HostProvided { interfaces: vec![] });
    (host, loads, fail_after)
}

#[test]
fn a_successful_reload_swaps_the_composition() {
    let dir = tempfile::tempdir().unwrap();
    let (host, loads, _fail) = host_with(&dir);

    host.compose().unwrap();
    let before = loads.load(Ordering::SeqCst);

    host.reload().unwrap();
    assert!(
        loads.load(Ordering::SeqCst) > before,
        "reload must re-read and re-load the guest"
    );

    // The host keeps serving from the new composition.
    assert!(host.checkout().is_ok());
}

#[test]
fn a_failed_reload_keeps_the_previous_composition_serving() {
    // CLNH-53: the host must not enter a broken state. This is the rule that
    // makes reload safe to wire to a signal — a bad deploy must not take the
    // process down with it.
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, fail_after) = host_with(&dir);

    host.compose().unwrap();
    assert!(host.checkout().is_ok(), "serving before the reload");

    // Every subsequent load fails.
    fail_after.store(0, Ordering::SeqCst);

    let err = host.reload().unwrap_err();
    assert!(err.to_string().contains("forced failure"), "{err}");

    // Still serving the old composition.
    assert!(
        host.checkout().is_ok(),
        "a failed reload must leave the previous composition active"
    );
}

#[test]
fn a_failed_reload_is_recorded_in_health() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, fail_after) = host_with(&dir);

    host.compose().unwrap();
    assert_eq!(host.health().last_reload_success, None);

    fail_after.store(0, Ordering::SeqCst);
    let _ = host.reload();

    let health = host.health();
    assert_eq!(health.last_reload_success, Some(false));
    assert!(health.last_reload_at.is_some());
    // Composition is still live despite the failure.
    assert!(health.composed);
}

#[test]
fn a_successful_reload_is_recorded_in_health() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, _fail) = host_with(&dir);

    host.compose().unwrap();
    host.reload().unwrap();

    let health = host.health();
    assert_eq!(health.last_reload_success, Some(true));
    assert!(health.last_reload_at.is_some());
}

#[test]
fn a_guest_deleted_between_reloads_fails_without_breaking_the_host() {
    // The realistic bad deploy: the artifact is gone when the signal arrives.
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, _fail) = host_with(&dir);

    host.compose().unwrap();
    std::fs::remove_file(dir.path().join("app.wasm")).unwrap();

    let err = host.reload().unwrap_err();
    assert!(err.to_string().contains("[guest] wasm"), "{err}");
    assert!(host.checkout().is_ok(), "still serving the old guest");
}

#[test]
fn in_flight_work_survives_a_reload() {
    // CLNH-50/52: a guard taken before the swap stays valid, and returning it
    // afterwards must not poison the new pool.
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, _fail) = host_with(&dir);

    host.compose().unwrap();
    let in_flight = host.checkout().unwrap();

    host.reload().unwrap();

    // The pre-reload guard is still usable and drops cleanly.
    drop(in_flight);

    // And the new composition serves.
    assert!(host.checkout().is_ok());
}

#[test]
fn reload_before_compose_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, fail_after) = host_with(&dir);
    fail_after.store(0, Ordering::SeqCst);

    // Nothing composed yet; reload must fail cleanly rather than unwrap a None.
    assert!(host.reload().is_err());
}

#[test]
fn repeated_reloads_do_not_leak_pools() {
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, _fail) = host_with(&dir);

    host.compose().unwrap();
    for _ in 0..5 {
        host.reload().unwrap();
    }

    let health = host.health();
    let pool = health.pool.expect("a pool after reload");
    // Only the newest pool is live; earlier generations were dropped.
    assert!(
        pool.instances_current <= 4,
        "instances leaked across reloads: {pool:?}"
    );
    assert!(host.checkout().is_ok());
}

#[test]
fn a_reload_that_fails_can_be_retried_successfully() {
    // An operator fixes the artifact and re-signals; the host must recover
    // rather than stay wedged in the failed state.
    let dir = tempfile::tempdir().unwrap();
    let (host, _loads, fail_after) = host_with(&dir);

    host.compose().unwrap();
    fail_after.store(0, Ordering::SeqCst);
    assert!(host.reload().is_err());

    fail_after.store(u32::MAX, Ordering::SeqCst);
    assert!(host.reload().is_ok(), "a fixed reload must succeed");
    assert_eq!(host.health().last_reload_success, Some(true));
}

/// Unused-parameter guard: the fake generation field exists so a future test
/// can assert which composition an instance came from.
#[test]
fn fake_instances_carry_their_generation() {
    let component = FakeComponent { generation: 7 };
    let mut instance = component.instantiate().unwrap();
    let concrete = instance
        .as_any_mut()
        .downcast_mut::<FakeInstance>()
        .expect("fake instance");
    assert_eq!(concrete.generation, 7);
    let _ = PathBuf::new();
}
