//! HCV-06 — the shared `host.wit` ↔ registration parity check.
//!
//! Platform 16 §16.14 obliges every host repo to CI-verify two things on every
//! commit: that `host.wit` exists at the repo root and parses, and that the
//! interfaces the host registers in its runtime import table match what
//! `host.wit` declares — no missing entries, no extra entries, no stubs.
//!
//! Per the clean-host-core spec (§507) this helper lives here rather than being
//! copied into each of the five host repos: the parity semantics are identical
//! everywhere, and duplicating them is explicitly called a defect.
//!
//! The host side of the check is a *declaration* the host makes about what it
//! registers. That declaration is only trustworthy if it is derived from the
//! same code path that does the registering — hosts build it by having their
//! linker-registration function report each interface it wires, so a
//! registration that never happens cannot appear in the list.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

/// One interface a host claims to register, plus how it is implemented.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Registration {
    /// `package:ns/iface@version`, matching the form used in `host.wit`.
    pub interface: String,
    /// Whether this is a real implementation. Stubs are a hard failure
    /// (Platform 16 §16.14, stub-import prohibition) — the correct response to
    /// an unimplemented interface is to omit it from `host.wit`, or to not
    /// register it so WASI catches it at load.
    pub kind: RegistrationKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegistrationKind {
    /// A working implementation.
    Real,
    /// A no-op or always-trapping shim. Never acceptable.
    Stub,
}

/// A parity failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParityViolation {
    /// `host.wit` declares it; nothing registers it.
    Declared { interface: String },
    /// Something registers it; `host.wit` does not declare it.
    Registered { interface: String },
    /// Registered as a no-op or throwing shim.
    Stubbed { interface: String },
    /// `host.wit` missing, misnamed, or unparseable.
    Wit { message: String },
}

impl fmt::Display for ParityViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Declared { interface } => write!(
                f,
                "host.wit declares `{interface}` but the runtime never registers it\n    \
                 fix: implement and register it, or remove it from host.wit"
            ),
            Self::Registered { interface } => write!(
                f,
                "the runtime registers `{interface}` but host.wit does not declare it\n    \
                 fix: declare it in host.wit, or stop registering it"
            ),
            Self::Stubbed { interface } => write!(
                f,
                "`{interface}` is registered as a no-op or throwing stub\n    \
                 fix: implement it properly, or omit it from host.wit and the linker so the \
                 gap surfaces at load time"
            ),
            Self::Wit { message } => write!(f, "host.wit: {message}"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParityReport {
    pub violations: Vec<ParityViolation>,
    pub declared: Vec<String>,
    pub registered: Vec<String>,
}

impl ParityReport {
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// Human-readable CI output.
    pub fn render(&self) -> String {
        if self.passed() {
            return format!(
                "HCV-06 parity: OK — {} interface(s) declared and registered\n",
                self.declared.len()
            );
        }
        let mut out = String::from("HCV-06 parity: FAILED\n\n");
        for v in &self.violations {
            out.push_str(&format!("  - {v}\n"));
        }
        out.push_str(&format!(
            "\n  declared in host.wit: {}\n  registered by host:   {}\n",
            self.declared.len(),
            self.registered.len()
        ));
        out
    }
}

/// Run the full HCV-06 check for a host repo.
///
/// `wit_path` is the repo-root `host.wit` (HCV-02); `registrations` is what the
/// host's linker wiring reports.
pub fn check(wit_path: impl AsRef<Path>, registrations: &[Registration]) -> ParityReport {
    let wit_path = wit_path.as_ref();
    let mut report = ParityReport::default();

    // Check 1 — presence and parse.
    let declared = match parse_host_wit(wit_path) {
        Ok(d) => d,
        Err(message) => {
            report.violations.push(ParityViolation::Wit { message });
            return report;
        }
    };

    // Check 2 — registration parity, in both directions.
    let registered: BTreeSet<String> = registrations.iter().map(|r| r.interface.clone()).collect();
    let declared_set: BTreeSet<String> = declared.iter().cloned().collect();

    for iface in declared_set.difference(&registered) {
        report.violations.push(ParityViolation::Declared {
            interface: iface.clone(),
        });
    }
    for iface in registered.difference(&declared_set) {
        report.violations.push(ParityViolation::Registered {
            interface: iface.clone(),
        });
    }
    for r in registrations {
        if r.kind == RegistrationKind::Stub {
            report.violations.push(ParityViolation::Stubbed {
                interface: r.interface.clone(),
            });
        }
    }

    report.declared = declared_set.into_iter().collect();
    report.registered = registered.into_iter().collect();
    report
}

/// Parse `host.wit` and return every interface the host's world exports.
///
/// A host's world *exports* what it provides to guests. Imports in that world
/// are what the host expects from the guest, so they are not part of the
/// provided surface and are excluded here.
pub fn parse_host_wit(path: impl AsRef<Path>) -> Result<Vec<String>, String> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(format!(
            "not found at `{}` — HCV-02 requires it at the repository root",
            path.display()
        ));
    }

    let mut resolve = wit_parser::Resolve::new();
    let package = resolve
        .push_path(path)
        .map_err(|e| format!("does not parse as valid WIT: {e:#}"))?
        .0;

    // Only the worlds this file's own package defines. A `use`d dependency may
    // drag other packages into the resolve graph; those are not this host's
    // declaration.
    let mut out = BTreeSet::new();
    for world_id in resolve.packages[package].worlds.values() {
        let world = &resolve.worlds[*world_id];
        for (key, item) in world.exports.iter() {
            if let wit_parser::WorldItem::Interface { id, .. } = item {
                if let Some(name) = interface_name(&resolve, *id, key) {
                    out.insert(name);
                }
            }
        }
    }

    Ok(out.into_iter().collect())
}

/// Render an interface as `package:ns/iface@version`.
fn interface_name(
    resolve: &wit_parser::Resolve,
    id: wit_parser::InterfaceId,
    key: &wit_parser::WorldKey,
) -> Option<String> {
    let iface = &resolve.interfaces[id];
    let pkg_id = iface.package?;
    let pkg = &resolve.packages[pkg_id];
    let iface_name = iface.name.clone().or_else(|| match key {
        wit_parser::WorldKey::Name(n) => Some(n.clone()),
        wit_parser::WorldKey::Interface(_) => None,
    })?;

    let mut s = format!("{}:{}/{}", pkg.name.namespace, pkg.name.name, iface_name);
    if let Some(v) = &pkg.name.version {
        s.push('@');
        s.push_str(&v.to_string());
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn wit_file(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.wit");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        (dir, path)
    }

    const WIT: &str = r#"
package clean:host@0.1.0;

interface routing {
    register: func(method: string, path: string);
}

interface request {
    method: func() -> string;
}

world server {
    export routing;
    export request;
}
"#;

    fn real(iface: &str) -> Registration {
        Registration {
            interface: iface.to_string(),
            kind: RegistrationKind::Real,
        }
    }

    #[test]
    fn parses_exported_interfaces_with_versions() {
        let (_d, path) = wit_file(WIT);
        let mut found = parse_host_wit(&path).unwrap();
        found.sort();
        assert_eq!(
            found,
            vec!["clean:host/request@0.1.0", "clean:host/routing@0.1.0"]
        );
    }

    #[test]
    fn exact_match_passes() {
        let (_d, path) = wit_file(WIT);
        let report = check(
            &path,
            &[
                real("clean:host/routing@0.1.0"),
                real("clean:host/request@0.1.0"),
            ],
        );
        assert!(report.passed(), "{}", report.render());
    }

    #[test]
    fn declared_but_unregistered_fails() {
        let (_d, path) = wit_file(WIT);
        let report = check(&path, &[real("clean:host/routing@0.1.0")]);
        assert!(!report.passed());
        assert!(report.violations.contains(&ParityViolation::Declared {
            interface: "clean:host/request@0.1.0".into()
        }));
    }

    #[test]
    fn registered_but_undeclared_fails() {
        let (_d, path) = wit_file(WIT);
        let report = check(
            &path,
            &[
                real("clean:host/routing@0.1.0"),
                real("clean:host/request@0.1.0"),
                real("clean:host/secret@0.1.0"),
            ],
        );
        assert!(!report.passed());
        assert!(report.violations.contains(&ParityViolation::Registered {
            interface: "clean:host/secret@0.1.0".into()
        }));
    }

    #[test]
    fn a_stub_registration_fails_even_when_names_line_up() {
        let (_d, path) = wit_file(WIT);
        let report = check(
            &path,
            &[
                real("clean:host/routing@0.1.0"),
                Registration {
                    interface: "clean:host/request@0.1.0".into(),
                    kind: RegistrationKind::Stub,
                },
            ],
        );
        assert!(!report.passed(), "stub must not pass parity");
        assert!(report.render().contains("no-op or throwing stub"));
    }

    #[test]
    fn missing_file_is_reported_as_an_hcv_02_violation() {
        let report = check("/nonexistent/host.wit", &[]);
        assert!(!report.passed());
        assert!(report.render().contains("HCV-02"));
    }

    #[test]
    fn unparseable_wit_is_reported() {
        let (_d, path) = wit_file("this is not wit {{{");
        let report = check(&path, &[]);
        assert!(!report.passed());
        assert!(report.render().contains("does not parse"));
    }
}
