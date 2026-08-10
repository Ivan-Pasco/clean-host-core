//! Bridge discovery and validation (§5.1–5.2, CLNH-17..24).
//!
//! For each `[bridges]` entry the library reads the `.wasm`, inspects its
//! component metadata, and checks it really exports the interface it was
//! configured to provide. Every failure here is a startup error naming the
//! bridge file and the rule it violated — CH-05 forbids composing a graph with
//! a bridge that only half-matches.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::validate::InterfaceRef;
use crate::HostError;

/// A bridge component that passed discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredBridge {
    /// The interface key from `[bridges]`, e.g. `clean:session/store`.
    pub interface: String,
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    /// Interfaces the component exports.
    pub exports: Vec<String>,
    /// Interfaces the component imports.
    pub imports: Vec<String>,
}

impl DiscoveredBridge {
    /// The version this bridge exports for its promised interface, if declared.
    pub fn exported_version(&self) -> Option<semver::Version> {
        let wanted = InterfaceRef::parse(&self.interface);
        self.exports
            .iter()
            .map(|e| InterfaceRef::parse(e))
            .find(|e| e.path == wanted.path)
            .and_then(|e| e.version)
    }
}

/// Inspect a component's imports and exports.
///
/// Supplied by the runtime adapter so this module stays engine-agnostic: the
/// library must not learn to parse Wasm itself (CH-06).
pub type Introspect = dyn Fn(&[u8]) -> Result<(Vec<String>, Vec<String>), String> + Send + Sync;

/// Convenience alias for adapters returning a boxed introspector.
pub type BoxedIntrospect = Box<Introspect>;

/// Read and validate every configured bridge.
pub fn discover(
    bridges: &BTreeMap<String, PathBuf>,
    introspect: &Introspect,
) -> Result<Vec<DiscoveredBridge>, HostError> {
    let mut discovered = Vec::new();

    for (interface, path) in bridges {
        discovered.push(discover_one(interface, path, introspect)?);
    }

    Ok(discovered)
}

fn discover_one(
    interface: &str,
    path: &Path,
    introspect: &Introspect,
) -> Result<DiscoveredBridge, HostError> {
    // CLNH-18: a missing file is a startup error naming the config key, so the
    // operator sees which `[bridges]` line is wrong rather than a bare IO error.
    if !path.exists() {
        return Err(HostError::BridgeDiscovery(format!(
            "bridge component not found\n  [bridges] \"{interface}\" = \"{}\"\n  \
             resolved to: {}",
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            path.display()
        )));
    }

    let bytes = std::fs::read(path).map_err(|e| {
        HostError::BridgeDiscovery(format!(
            "cannot read bridge `{interface}` at {}: {e}",
            path.display()
        ))
    })?;

    // CLNH-19: not a component is a startup error.
    let (imports, exports) = introspect(&bytes).map_err(|e| {
        HostError::BridgeDiscovery(format!(
            "bridge `{interface}` at {} is not a valid Component Model component: {e}",
            path.display()
        ))
    })?;

    let bridge = DiscoveredBridge {
        interface: interface.to_string(),
        path: path.to_path_buf(),
        bytes,
        exports,
        imports,
    };

    // CLNH-20: the file must actually export what it was configured to provide.
    // Without this, a typo in a path composes silently and fails at the first
    // call instead of at startup.
    let wanted = InterfaceRef::parse(interface);
    let exports_it = bridge
        .exports
        .iter()
        .any(|e| InterfaceRef::parse(e).path == wanted.path);

    if !exports_it {
        return Err(HostError::BridgeDiscovery(format!(
            "bridge at {} does not export `{interface}`\n  it exports: {}\n  \
             check the `[bridges]` key matches the component",
            bridge.path.display(),
            if bridge.exports.is_empty() {
                "(nothing)".to_string()
            } else {
                bridge.exports.join(", ")
            }
        )));
    }

    Ok(bridge)
}

/// Check a bridge's exported version against what the guest asked for.
///
/// CLNH-21: the host's advertised version must fall inside the range the
/// bridge exports. Pre-1.0 semver makes a minor bump breaking.
pub fn check_version(bridge: &DiscoveredBridge, required: &InterfaceRef) -> Result<(), HostError> {
    let (Some(have), Some(want)) = (bridge.exported_version(), required.version.clone()) else {
        // An unversioned reference on either side cannot be checked; the
        // Moment 3 comparison against host.wit still applies.
        return Ok(());
    };

    let compatible = if want.major == 0 {
        want.major == have.major && want.minor == have.minor && have.patch >= want.patch
    } else {
        want.major == have.major && (have.minor, have.patch) >= (want.minor, want.patch)
    };

    if compatible {
        Ok(())
    } else {
        Err(HostError::BridgeDiscovery(format!(
            "bridge `{}` version mismatch\n  guest requires: {}@{want}\n  \
             bridge exports: {}@{have}\n  (semver-incompatible)",
            bridge.interface, required.path, bridge.interface
        )))
    }
}

/// Bridges MUST NOT import interfaces outside their allowlist (CLNH-22).
///
/// A session bridge reaching for `wasi:http/outgoing-handler` is either a
/// mistake or an exfiltration path; either way the operator should hear about
/// it at startup rather than discover it in production traffic.
pub fn check_imports(bridge: &DiscoveredBridge) -> Result<(), HostError> {
    let disallowed: Vec<&String> = bridge
        .imports
        .iter()
        .filter(|i| !import_is_allowed(i))
        .collect();

    if disallowed.is_empty() {
        Ok(())
    } else {
        Err(HostError::BridgeDiscovery(format!(
            "bridge `{}` imports interfaces outside the bridge allowlist: {}\n  \
             bridges may import the WASI baseline, `clean:host/*`, and host-side \
             envelope interfaces only",
            bridge.interface,
            disallowed
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

/// What a bridge is permitted to import.
fn import_is_allowed(interface: &str) -> bool {
    let path = InterfaceRef::parse(interface).path;

    // The standard stack every component gets (CH-03).
    if path.starts_with("wasi:") || path.starts_with("clean:host/") {
        return true;
    }

    // Host-side envelopes a bridge legitimately calls back into: the realtime
    // bridge imports `clean:realtime/sockets`, the session bridge may import
    // `clean:session/http-envelope`. These are implemented by the host, so
    // importing them is the documented inverse-direction pattern.
    matches!(
        path.as_str(),
        "clean:realtime/sockets" | "clean:session/http-envelope"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fake_component(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"\0asm\x0d\0\x01\0").unwrap();
        path
    }

    /// An introspector returning fixed answers.
    fn introspector(imports: Vec<&'static str>, exports: Vec<&'static str>) -> Box<Introspect> {
        Box::new(move |_bytes: &[u8]| {
            Ok((
                imports.iter().map(|s| s.to_string()).collect(),
                exports.iter().map(|s| s.to_string()).collect(),
            ))
        })
    }

    #[test]
    fn a_bridge_exporting_its_interface_is_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_component(dir.path(), "session.wasm");
        let mut bridges = BTreeMap::new();
        bridges.insert("clean:session/store".to_string(), path);

        let introspect = introspector(vec![], vec!["clean:session/store@1.1.0"]);
        let found = discover(&bridges, &*introspect).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].interface, "clean:session/store");
        assert_eq!(
            found[0].exported_version(),
            Some(semver::Version::new(1, 1, 0))
        );
    }

    #[test]
    fn a_missing_bridge_file_names_the_config_key() {
        let mut bridges = BTreeMap::new();
        bridges.insert(
            "clean:session/store".to_string(),
            PathBuf::from("/nope/session.wasm"),
        );

        let introspect = introspector(vec![], vec![]);
        let err = discover(&bridges, &*introspect).unwrap_err().to_string();
        assert!(err.contains("[bridges]"), "{err}");
        assert!(err.contains("clean:session/store"), "{err}");
    }

    #[test]
    fn a_bridge_that_does_not_export_its_interface_is_rejected() {
        // A path typo pointing at the wrong component composes silently
        // otherwise, and fails at the first call instead of at startup.
        let dir = tempfile::tempdir().unwrap();
        let path = fake_component(dir.path(), "wrong.wasm");
        let mut bridges = BTreeMap::new();
        bridges.insert("clean:session/store".to_string(), path);

        let introspect = introspector(vec![], vec!["clean:kv/store@1.0.0"]);
        let err = discover(&bridges, &*introspect).unwrap_err().to_string();
        assert!(err.contains("does not export"), "{err}");
        assert!(err.contains("clean:kv/store"), "{err}");
    }

    #[test]
    fn a_non_component_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_component(dir.path(), "core.wasm");
        let mut bridges = BTreeMap::new();
        bridges.insert("clean:session/store".to_string(), path);

        let introspect: Box<Introspect> =
            Box::new(|_| Err("core module, not a component".to_string()));
        let err = discover(&bridges, &*introspect).unwrap_err().to_string();
        assert!(
            err.contains("not a valid Component Model component"),
            "{err}"
        );
    }

    fn bridge_with(imports: Vec<&str>) -> DiscoveredBridge {
        DiscoveredBridge {
            interface: "clean:session/store".into(),
            path: PathBuf::from("/x/session.wasm"),
            bytes: vec![],
            exports: vec!["clean:session/store@1.1.0".into()],
            imports: imports.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn the_wasi_baseline_and_clean_host_are_allowed_imports() {
        let bridge = bridge_with(vec![
            "wasi:clocks/monotonic-clock@0.3.0",
            "wasi:sockets/tcp@0.3.0",
            "clean:host/config@1.0.0",
        ]);
        assert!(check_imports(&bridge).is_ok());
    }

    #[test]
    fn a_host_side_envelope_is_an_allowed_import() {
        // The realtime bridge imports the host's sockets envelope by design.
        let bridge = bridge_with(vec!["clean:realtime/sockets@1.1.0"]);
        assert!(check_imports(&bridge).is_ok());
    }

    #[test]
    fn an_unexpected_capability_import_is_rejected() {
        // A session bridge reaching for outbound HTTP is either a mistake or an
        // exfiltration path; the operator hears about it at startup.
        let bridge = bridge_with(vec!["clean:data/store@1.0.0"]);
        let err = check_imports(&bridge).unwrap_err().to_string();
        assert!(err.contains("outside the bridge allowlist"), "{err}");
        assert!(err.contains("clean:data/store"), "{err}");
    }

    #[test]
    fn a_compatible_version_passes() {
        let bridge = bridge_with(vec![]);
        let required = InterfaceRef::parse("clean:session/store@1.1.0");
        assert!(check_version(&bridge, &required).is_ok());
    }

    #[test]
    fn a_newer_bridge_patch_satisfies_an_older_requirement() {
        let mut bridge = bridge_with(vec![]);
        bridge.exports = vec!["clean:session/store@1.1.4".into()];
        let required = InterfaceRef::parse("clean:session/store@1.1.0");
        assert!(check_version(&bridge, &required).is_ok());
    }

    #[test]
    fn a_pre_1_0_minor_bump_is_breaking() {
        let mut bridge = bridge_with(vec![]);
        bridge.exports = vec!["clean:session/store@0.2.0".into()];
        let required = InterfaceRef::parse("clean:session/store@0.1.0");
        let err = check_version(&bridge, &required).unwrap_err().to_string();
        assert!(err.contains("semver-incompatible"), "{err}");
    }

    #[test]
    fn a_major_mismatch_is_rejected() {
        let mut bridge = bridge_with(vec![]);
        bridge.exports = vec!["clean:session/store@2.0.0".into()];
        let required = InterfaceRef::parse("clean:session/store@1.1.0");
        assert!(check_version(&bridge, &required).is_err());
    }
}
