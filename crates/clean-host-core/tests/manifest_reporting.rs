//! The manifest must describe what is live, not what was configured
//! (CLNH-44..47).
//!
//! `host-capabilities.cln` is what `cln`, the LSP, the framework's capability
//! checker and AI assistants read to decide what a deployment can actually do.
//! A bridge listed in `[bridges]` that the guest never imports is filtered out
//! before composition, so reporting it as `active` would tell every one of
//! those readers a capability is available when nothing was wired for it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use clean_host_core::runtime::{
    Instance, LoadedComponent, RuntimeCapabilities, RuntimeError, WasmRuntime,
};
use clean_host_core::{Host, HostConfig, HostProvided};

/// A bridge exporting `clean:session/store@0.1.0`.
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

/// A guest importing that store.
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

/// A runtime whose guest imports exactly what the test asks it to.
struct FakeRuntime {
    guest_imports: Vec<String>,
    bridge_exports: Vec<String>,
    loads: Arc<AtomicU32>,
}

struct FakeComponent {
    imports: Vec<String>,
}

struct FakeInstance;

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
        Ok(Box::new(FakeInstance))
    }
    fn imports(&self) -> Vec<String> {
        self.imports.clone()
    }
    fn exports(&self) -> Vec<String> {
        vec!["handle".to_string()]
    }
}

impl WasmRuntime for FakeRuntime {
    fn load(&self, _bytes: &[u8]) -> Result<Box<dyn LoadedComponent>, RuntimeError> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeComponent {
            imports: self.guest_imports.clone(),
        }))
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
        let exports = self.bridge_exports.clone();
        Box::new(move |_| Ok((vec![], exports.clone())))
    }
}

/// Build a host whose guest imports `guest_imports` and whose `[bridges]`
/// block configures `bridge_iface`.
fn host_with(
    dir: &tempfile::TempDir,
    guest_imports: &[&str],
    bridge_iface: &str,
) -> (Host, PathBuf) {
    // Real component bytes: WAC parses the bridge for real when it composes,
    // so a header-only stand-in fails at the alias step rather than at any
    // assertion this test is making.
    let guest = dir.path().join("app.wasm");
    std::fs::write(&guest, wat::parse_str(GUEST_WAT).unwrap()).unwrap();

    let bridge = dir.path().join("session-bridge.wasm");
    std::fs::write(&bridge, wat::parse_str(BRIDGE_WAT).unwrap()).unwrap();

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
world = "server"

[runtime]
instances-min = 1
instances-max = 2

[bridges]
"{}" = "{}"
"#,
            guest.display(),
            bridge_iface,
            bridge.display()
        ),
        dir.path().join("host.toml"),
    )
    .unwrap();

    let manifest_path = config.manifest_path();

    let runtime = FakeRuntime {
        guest_imports: guest_imports.iter().map(|s| s.to_string()).collect(),
        bridge_exports: vec![bridge_iface.to_string()],
        loads: Arc::new(AtomicU32::new(0)),
    };

    let mut host = Host::new(config, Box::new(runtime)).unwrap();
    host.set_host_provided(HostProvided { interfaces: vec![] });
    (host, manifest_path)
}

/// A bridge the guest actually imports is composed, and reported `active`.
#[test]
fn a_composed_bridge_is_reported_active() {
    let dir = tempfile::tempdir().unwrap();
    let (host, manifest_path) = host_with(
        &dir,
        &["clean:session/store@0.1.0"],
        "clean:session/store@0.1.0",
    );

    host.compose().unwrap();
    host.emit_manifest().unwrap();

    let text = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(
        text.contains("clean:session/store"),
        "the composed bridge must appear in the manifest:\n{text}"
    );
    assert!(
        text.contains("active"),
        "a bridge the guest imports must be reported active:\n{text}"
    );
}

/// A bridge the guest never imports is filtered out before composition, so the
/// manifest must not claim it is live.
///
/// This is the case that was previously reported `active` from the config map
/// alone: the operator's `[bridges]` line was read back to them as a live
/// capability even though nothing was wired for it.
#[test]
fn a_configured_but_unimported_bridge_is_not_reported_active() {
    let dir = tempfile::tempdir().unwrap();
    // The guest imports nothing, so the configured bridge is never composed.
    let (host, manifest_path) = host_with(&dir, &[], "clean:session/store@0.1.0");

    host.compose().unwrap();
    host.emit_manifest().unwrap();

    let text = std::fs::read_to_string(&manifest_path).unwrap();

    let line = text
        .lines()
        .find(|l| l.contains("clean:session/store"))
        .unwrap_or_else(|| panic!("the configured bridge must still be listed:\n{text}"));

    assert!(
        !line.contains("\"active\"") && !line.contains("= active"),
        "an uncomposed bridge must not be reported active, got: {line}"
    );
    assert!(
        text.contains("unavailable"),
        "an uncomposed bridge must be reported unavailable:\n{text}"
    );
    assert!(
        text.contains("does not import"),
        "the manifest must explain why the capability is not live:\n{text}"
    );
}
