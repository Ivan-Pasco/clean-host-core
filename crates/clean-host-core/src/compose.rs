//! Component composition (§5.3, CLNH-25/26) and the capability-grant rules
//! that gate it (CH-04, SRVH-01/02).
//!
//! Composition wires the guest's imports to the composed bridges' exports and
//! produces one component ready to instantiate. WAC (the Bytecode Alliance's
//! composition tool) performs the graph surgery; this module decides *what*
//! graph to build and refuses to build an incomplete one.

use std::collections::BTreeMap;

use wac_graph::types::Package;
use wac_graph::CompositionGraph;

use crate::bridge::DiscoveredBridge;
use crate::validate::{is_ambient_interface, InterfaceRef};
use crate::HostError;

/// What the guest asked for that nothing provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsatisfiedImport {
    pub interface: String,
    /// The `[bridges]` key an operator would add to fix it.
    pub suggested_key: String,
}

/// Check that every capability the guest imports is actually provided.
///
/// SRVH-01/CH-04: capabilities are opt-in, and only what is imported is
/// granted. SRVH-02: absent configuration means the capability is off — so an
/// import with no matching `[bridges]` entry is a startup error naming the
/// missing key, not a silently disabled feature that fails in production on a
/// code path nobody exercised in staging.
pub fn check_capabilities(
    guest_imports: &[String],
    host_provided: &[String],
    bridges: &[DiscoveredBridge],
) -> Result<(), HostError> {
    let mut satisfied: Vec<String> = host_provided
        .iter()
        .map(|i| InterfaceRef::parse(i).path)
        .collect();
    satisfied.extend(
        bridges
            .iter()
            .map(|b| InterfaceRef::parse(&b.interface).path),
    );

    let unsatisfied: Vec<UnsatisfiedImport> = guest_imports
        .iter()
        .filter_map(|import| {
            let parsed = InterfaceRef::parse(import);
            // Ambient interfaces are always provided and never need a bridge.
            if is_ambient_interface(&parsed.path) || satisfied.contains(&parsed.path) {
                return None;
            }
            Some(UnsatisfiedImport {
                interface: import.clone(),
                suggested_key: parsed.path.clone(),
            })
        })
        .collect();

    if unsatisfied.is_empty() {
        return Ok(());
    }

    let mut message = String::from("the guest imports capabilities this host does not provide:\n");
    for u in &unsatisfied {
        message.push_str(&format!(
            "  - {}\n    add to host.toml:\n      [bridges]\n      \"{}\" = \"./bridges/<component>.wasm\"\n",
            u.interface, u.suggested_key
        ));
    }
    message.push_str(
        "\nEvery capability a guest imports must be explicitly configured; an \
         absent entry means the capability is off (SRVH-02).",
    );

    Err(HostError::Validation(message))
}

/// Bridges that were configured but that the guest never imports.
///
/// CH-04 says an unimported capability is not instantiated. Composing it anyway
/// would grant reach nothing asked for, so it is dropped — and reported, since
/// a configured-but-unused bridge is usually a stale config line.
pub fn unused_bridges(guest_imports: &[String], bridges: &[DiscoveredBridge]) -> Vec<String> {
    let wanted: Vec<String> = guest_imports
        .iter()
        .map(|i| InterfaceRef::parse(i).path)
        .collect();

    bridges
        .iter()
        .filter(|b| !wanted.contains(&InterfaceRef::parse(&b.interface).path))
        .map(|b| b.interface.clone())
        .collect()
}

/// Compose the guest with its bridges into a single component.
///
/// CLNH-25: WAC performs the composition and discharges COMP-09..COMP-12.
/// CLNH-26: the result is a byte buffer held in memory.
///
/// With no bridges to wire, the guest is already the composed component and is
/// returned unchanged — running WAC on a single node would only risk changing
/// bytes that nothing asked to change.
pub fn compose(
    guest: &[u8],
    guest_name: &str,
    guest_exports: &[String],
    bridges: &[DiscoveredBridge],
) -> Result<Vec<u8>, HostError> {
    if bridges.is_empty() {
        return Ok(guest.to_vec());
    }

    let mut graph = CompositionGraph::new();

    // Register the guest.
    let guest_pkg = Package::from_bytes(guest_name, None, guest.to_vec(), graph.types_mut())
        .map_err(|e| HostError::Composition(format!("guest is not a valid component: {e:#}")))?;
    let guest_id = graph.register_package(guest_pkg).map_err(|e| {
        HostError::Composition(format!("cannot register guest for composition: {e:#}"))
    })?;

    // Register every bridge and instantiate it.
    let mut bridge_instances = BTreeMap::new();
    for bridge in bridges {
        let name = package_name(&bridge.interface);
        let pkg = Package::from_bytes(&name, None, bridge.bytes.clone(), graph.types_mut())
            .map_err(|e| {
                HostError::Composition(format!(
                    "bridge `{}` at {} is not a valid component: {e:#}",
                    bridge.interface,
                    bridge.path.display()
                ))
            })?;
        let pkg_id = graph.register_package(pkg).map_err(|e| {
            HostError::Composition(format!(
                "cannot register bridge `{}`: {e:#}",
                bridge.interface
            ))
        })?;

        let instance = graph.instantiate(pkg_id);
        bridge_instances.insert(bridge.interface.clone(), (pkg_id, instance));
    }

    // Wire each guest import to the bridge export that satisfies it.
    let guest_instance = graph.instantiate(guest_id);
    for bridge in bridges {
        // Both lookups below should be unreachable: every bridge was inserted
        // into the map immediately above, and `bridge::discover` rejects a
        // component that does not export its promised interface. They are hard
        // errors rather than `continue`s anyway, because skipping one silently
        // yields a component that loads and runs with a capability quietly
        // missing — the silent fallback CH-05 forbids, and the failure mode is
        // invisible until a guest calls the interface that was never wired.
        let Some((_, bridge_instance)) = bridge_instances.get(&bridge.interface) else {
            return Err(HostError::Composition(format!(
                "internal: bridge `{}` was registered for composition but is missing \
                 from the instance map",
                bridge.interface
            )));
        };

        // The export name as the bridge actually spells it, which may carry a
        // version suffix the `[bridges]` key does not.
        let wanted = InterfaceRef::parse(&bridge.interface).path;
        let Some(export_name) = bridge
            .exports
            .iter()
            .find(|e| InterfaceRef::parse(e).path == wanted)
        else {
            return Err(HostError::Composition(format!(
                "bridge `{}` at {} does not export `{wanted}`; it exports {:?}",
                bridge.interface,
                bridge.path.display(),
                bridge.exports
            )));
        };

        let export = graph
            .alias_instance_export(*bridge_instance, export_name)
            .map_err(|e| {
                HostError::Composition(format!(
                    "cannot alias `{export_name}` from bridge `{}`: {e:#}",
                    bridge.interface
                ))
            })?;

        graph
            .set_instantiation_argument(guest_instance, export_name, export)
            .map_err(|e| {
                HostError::Composition(format!("cannot wire `{export_name}` into the guest: {e:#}"))
            })?;
    }

    // Re-export whatever the guest exports so the host can still call it.
    // The names come from discovery rather than from the graph: the caller
    // already introspected the component, and re-deriving them here would mean
    // this module learning to read component types.
    // These names were read off this very component's type section, so every
    // one of them must alias and re-export. Swallowing a failure here is how a
    // composition ends up with nothing the host can call: the bytes encode,
    // startup succeeds, and the first invocation fails on a missing export.
    for export in guest_exports {
        let value = graph
            .alias_instance_export(guest_instance, export)
            .map_err(|e| {
                HostError::Composition(format!(
                    "guest declares export `{export}` but it cannot be aliased \
                     for re-export: {e:#}"
                ))
            })?;
        graph.export(value, export).map_err(|e| {
            HostError::Composition(format!(
                "cannot re-export `{export}` from the composed component: {e:#}"
            ))
        })?;
    }

    graph.encode(Default::default()).map_err(|e| {
        // COMP-22: WAC's own checks discharge COMP-09..COMP-12, and its error
        // is surfaced rather than replaced — it names the unresolved import.
        HostError::Composition(format!("composition failed: {e:#}"))
    })
}

/// A WAC package name for a bridge, derived from its interface.
///
/// WAC requires `namespace:name`; `clean:session/store` becomes
/// `clean:session-store`.
fn package_name(interface: &str) -> String {
    let path = InterfaceRef::parse(interface).path;
    match path.split_once(':') {
        Some((ns, rest)) => format!("{ns}:{}", rest.replace('/', "-")),
        None => format!("bridge:{}", path.replace('/', "-")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn bridge(interface: &str) -> DiscoveredBridge {
        DiscoveredBridge {
            interface: interface.to_string(),
            path: PathBuf::from("/x/b.wasm"),
            bytes: vec![],
            exports: vec![format!("{interface}@1.0.0")],
            imports: vec![],
        }
    }

    /// A host-provided interface satisfies the import via the `host_provided`
    /// list, not via the ambient short-circuit.
    ///
    /// Deliberately spelled with a non-`clean:host/` package: anything under
    /// `clean:host/` is ambient (CH-03) and would pass this assertion even if
    /// `host_provided` were empty, which would make the second argument dead
    /// and the test vacuous.
    #[test]
    fn a_guest_importing_only_host_interfaces_is_satisfied() {
        let result = check_capabilities(
            &["clean:realtime/sockets@0.1.0".into()],
            &["clean:realtime/sockets@0.1.0".into()],
            &[],
        );
        assert!(result.is_ok());

        // The same import with nothing provided must fail — proving the pass
        // above came from `host_provided` and not from ambience.
        assert!(check_capabilities(&["clean:realtime/sockets@0.1.0".into()], &[], &[]).is_err());
    }

    #[test]
    fn ambient_imports_never_need_a_bridge() {
        // CH-03: the WASI stack and clean:host/* are always provided.
        let result = check_capabilities(
            &[
                "wasi:clocks/monotonic-clock@0.3.0".into(),
                "clean:host/log@1.0.0".into(),
            ],
            &[],
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn an_unsatisfied_import_names_the_missing_bridges_key() {
        // SRVH-02: the diagnostic must say which key to add, or the operator
        // is left guessing at the exact interface spelling.
        let err = check_capabilities(&["clean:session/store@1.1.0".into()], &[], &[])
            .unwrap_err()
            .to_string();

        assert!(err.contains("clean:session/store"), "{err}");
        assert!(err.contains("[bridges]"), "{err}");
        assert!(err.contains("SRVH-02"), "{err}");
    }

    #[test]
    fn a_configured_bridge_satisfies_the_import() {
        let result = check_capabilities(
            &["clean:session/store@1.1.0".into()],
            &[],
            &[bridge("clean:session/store")],
        );
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn several_unsatisfied_imports_are_all_reported() {
        // Reporting one at a time turns a config fix into a guessing game.
        let err = check_capabilities(
            &[
                "clean:session/store@1.1.0".into(),
                "clean:kv/store@1.0.0".into(),
            ],
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("clean:session/store"), "{err}");
        assert!(err.contains("clean:kv/store"), "{err}");
    }

    #[test]
    fn a_bridge_the_guest_does_not_import_is_reported_as_unused() {
        // CH-04: an unimported capability is not granted.
        let unused = unused_bridges(
            &["clean:session/store@1.1.0".into()],
            &[bridge("clean:session/store"), bridge("clean:kv/store")],
        );
        assert_eq!(unused, vec!["clean:kv/store"]);
    }

    #[test]
    fn composing_without_bridges_returns_the_guest_unchanged() {
        let guest = b"\0asm\x0d\0\x01\0".to_vec();
        let composed = compose(&guest, "app", &[], &[]).unwrap();
        assert_eq!(composed, guest, "a bridgeless guest must not be rewritten");
    }

    #[test]
    fn package_names_are_wac_safe() {
        assert_eq!(package_name("clean:session/store"), "clean:session-store");
        assert_eq!(
            package_name("clean:realtime/rooms@1.1.0"),
            "clean:realtime-rooms"
        );
    }
}
