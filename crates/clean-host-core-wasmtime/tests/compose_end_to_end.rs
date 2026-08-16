//! End-to-end coverage for the paths every deployment actually takes:
//! discover a real bridge, compose it into a real guest with WAC, load the
//! result under Wasmtime, and instantiate it.
//!
//! Everything else in this workspace tests those steps against a `FakeRuntime`
//! and an eight-byte stand-in for a guest, which cannot fail the way a real
//! component can. The fixtures below are `.wat` rather than committed `.wasm`
//! so they stay readable in a diff and can be corrected by hand; `wat` compiles
//! them at test time.

use clean_host_core::bridge::DiscoveredBridge;
use clean_host_core::compose::compose;
use clean_host_core::runtime::WasmRuntime;
use clean_host_core_wasmtime::{EngineConfig, WasmtimeRuntime};
use std::path::PathBuf;

/// A bridge exporting `clean:session/store@0.1.0` with a single `put`.
const BRIDGE_WAT: &str = r#"
(component
  (core module $m
    (func (export "put") (param i32) (result i32) local.get 0)
  )
  (core instance $i (instantiate $m))
  (func $put (param "key" s32) (result s32)
    (canon lift (core func $i "put")))
  (instance $store (export "put" (func $put)))
  (export "clean:session/store@0.1.0" (instance $store))
)
"#;

/// A guest importing that store and exporting a handler of its own.
const GUEST_WAT: &str = r#"
(component
  (import "clean:session/store@0.1.0" (instance $store
    (export "put" (func (param "key" s32) (result s32)))))
  (core module $m
    (func (export "handle") (result i32) i32.const 7)
  )
  (core instance $i (instantiate $m))
  (func $handle (result s32) (canon lift (core func $i "handle")))
  (instance $h (export "handle" (func $handle)))
  (export "clean:host/handler@0.1.0" (instance $h))
)
"#;

fn runtime() -> WasmtimeRuntime {
    WasmtimeRuntime::new(EngineConfig::default()).expect("engine builds")
}

fn bridge_fixture(bytes: Vec<u8>, exports: Vec<String>, imports: Vec<String>) -> DiscoveredBridge {
    DiscoveredBridge {
        interface: "clean:session/store@0.1.0".into(),
        path: PathBuf::from("/fixtures/session-bridge.wasm"),
        bytes,
        exports,
        imports,
    }
}

/// The introspector must read a real component's contract off its type section.
#[test]
fn introspection_reads_a_real_bridge_and_guest_contract() {
    let rt = runtime();
    let introspect = rt.introspector();

    let bridge = wat::parse_str(BRIDGE_WAT).expect("bridge wat compiles");
    let (b_imports, b_exports) = introspect(&bridge).expect("bridge is a component");
    assert!(
        b_exports.iter().any(|e| e.contains("clean:session/store")),
        "bridge must export the store interface, got {b_exports:?}"
    );
    assert!(
        b_imports.is_empty(),
        "this bridge imports nothing, got {b_imports:?}"
    );

    let guest = wat::parse_str(GUEST_WAT).expect("guest wat compiles");
    let (g_imports, g_exports) = introspect(&guest).expect("guest is a component");
    assert!(
        g_imports.iter().any(|i| i.contains("clean:session/store")),
        "guest must import the store interface, got {g_imports:?}"
    );
    assert!(
        g_exports.iter().any(|e| e.contains("clean:host/handler")),
        "guest must export its handler, got {g_exports:?}"
    );
}

/// The path a deployment with one bridge takes: WAC wires the bridge export
/// into the guest import and the result loads and instantiates.
#[test]
fn a_guest_and_its_bridge_compose_load_and_instantiate() {
    let rt = runtime();
    let introspect = rt.introspector();

    let bridge_bytes = wat::parse_str(BRIDGE_WAT).expect("bridge wat compiles");
    let guest_bytes = wat::parse_str(GUEST_WAT).expect("guest wat compiles");

    let (b_imports, b_exports) = introspect(&bridge_bytes).expect("bridge is a component");
    let (g_imports, g_exports) = introspect(&guest_bytes).expect("guest is a component");

    // Before composition the guest carries the store as an unresolved import.
    // Asserting this here is what gives the post-composition check its meaning:
    // without it, "no store import" could just mean the guest never had one.
    assert!(
        g_imports.iter().any(|i| i.contains("clean:session/store")),
        "fixture guest must start with an unresolved store import, got {g_imports:?}"
    );

    let bridge = bridge_fixture(bridge_bytes, b_exports, b_imports);
    let composed = compose(&guest_bytes, "app", &g_exports, std::slice::from_ref(&bridge))
        .expect("composition succeeds");

    assert_ne!(
        composed, guest_bytes,
        "composing a bridge in must change the component bytes"
    );

    // The composed component is self-contained: it loads and instantiates.
    let loaded = rt.load(&composed).expect("composed component loads");
    assert!(
        loaded.exports().iter().any(|e| e.contains("clean:host/handler")),
        "the guest's own export must survive composition, got {:?}",
        loaded.exports()
    );
    assert!(
        !loaded.imports().iter().any(|i| i.contains("clean:session/store")),
        "the store import must be satisfied by the composed bridge, got {:?}",
        loaded.imports()
    );

    loaded.instantiate().expect("composed component instantiates");
}

/// A bridge that does not export the interface it was configured for is a
/// composition error, not a silent skip (CH-05).
///
/// Skipping it would encode a component that loads and starts with the
/// capability quietly absent, failing only when a guest first calls it.
#[test]
fn a_bridge_missing_its_promised_export_is_a_composition_error() {
    let guest_bytes = wat::parse_str(GUEST_WAT).expect("guest wat compiles");
    let bridge_bytes = wat::parse_str(BRIDGE_WAT).expect("bridge wat compiles");

    // Discovery recorded an export list that does not contain the promised
    // interface — the shape a drifted or mislabelled bridge produces.
    let bridge = bridge_fixture(bridge_bytes, vec!["clean:kv/store@0.1.0".into()], vec![]);

    let err = compose(&guest_bytes, "app", &[], std::slice::from_ref(&bridge))
        .expect_err("a bridge that does not export its interface must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("clean:session/store"),
        "the error must name the interface that was promised, got: {msg}"
    );
}

/// A bridge whose bytes are not a component is refused with a message naming
/// the bridge, not a bare WAC panic.
#[test]
fn a_bridge_that_is_not_a_component_is_refused_by_name() {
    let guest_bytes = wat::parse_str(GUEST_WAT).expect("guest wat compiles");
    let core_module = wat::parse_str(r#"(module (func (export "put")))"#).expect("core wat compiles");

    let bridge = bridge_fixture(
        core_module,
        vec!["clean:session/store@0.1.0".into()],
        vec![],
    );

    let err = compose(&guest_bytes, "app", &[], std::slice::from_ref(&bridge))
        .expect_err("a core module is not a valid bridge component");
    let msg = err.to_string();
    assert!(
        msg.contains("clean:session/store"),
        "the error must name the offending bridge, got: {msg}"
    );
}
