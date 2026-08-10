//! Moment 3 — the load-time host contract check (HCV-01, Platform 16 §16.4).
//!
//! Before a guest is instantiated, its imports are compared against the
//! concrete host's `host.wit`. A mismatch is refused with structured error
//! `COM017`, never a bare WASM trap. `clean-host-core` performs this on behalf
//! of every concrete host (host-core spec §504).
//!
//! Compliance is presence + version compatibility (HCV-03). Signature identity
//! is the third leg; it requires resolving both sides' full type graphs and is
//! tracked as remaining Phase 3 work — see [`ComplianceReport::signature_checked`].

use std::collections::BTreeMap;
use std::fmt;

/// A parsed WIT interface reference: `package:ns/iface@version`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InterfaceRef {
    /// Everything before `@` — e.g. `clean:http/routing`.
    pub path: String,
    /// The version, if the reference carried one.
    pub version: Option<semver::Version>,
}

impl InterfaceRef {
    pub fn parse(s: &str) -> Self {
        match s.split_once('@') {
            Some((path, ver)) => Self {
                path: path.to_string(),
                version: semver::Version::parse(ver).ok(),
            },
            None => Self {
                path: s.to_string(),
                version: None,
            },
        }
    }

    /// Pre-1.0 semver: a `0.x` bump is breaking, so compatibility requires the
    /// same major *and* minor. At `>=1.0`, the major must match and the host
    /// must be at least as new as the guest asked for.
    fn compatible_with(&self, guest: &InterfaceRef) -> bool {
        match (&self.version, &guest.version) {
            // An unversioned reference on either side cannot be checked.
            (None, _) | (_, None) => true,
            (Some(host), Some(want)) => {
                if want.major != host.major {
                    return false;
                }
                if want.major == 0 {
                    want.minor == host.minor && host.patch >= want.patch
                } else {
                    (host.minor, host.patch) >= (want.minor, want.patch)
                }
            }
        }
    }
}

impl fmt::Display for InterfaceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(v) => write!(f, "{}@{}", self.path, v),
            None => write!(f, "{}", self.path),
        }
    }
}

/// Why a guest was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The host declares nothing at this interface path.
    MissingInterface { required: InterfaceRef },
    /// The path exists but at a semver-incompatible version.
    VersionMismatch {
        required: InterfaceRef,
        provided: InterfaceRef,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInterface { required } => {
                write!(
                    f,
                    "component requires: {required}\n       host provides:      (nothing at this interface)"
                )
            }
            Self::VersionMismatch { required, provided } => write!(
                f,
                "component requires: {required}\n       host provides:      {provided}\n       (semver-incompatible)"
            ),
        }
    }
}

/// The outcome of a Moment 3 check.
#[derive(Debug, Clone)]
pub struct ComplianceReport {
    pub violations: Vec<Violation>,
    /// Host interfaces the guest never imports. Harmless — compliance is
    /// one-way (HCV-03) — but surfaced so `--check --strict` can report them.
    pub unused_host_interfaces: Vec<InterfaceRef>,
    /// False until signature identity is implemented (Phase 3). Recorded so no
    /// caller can mistake a presence-and-version pass for a full HCV-03 pass.
    pub signature_checked: bool,
}

impl ComplianceReport {
    pub fn is_compliant(&self) -> bool {
        self.violations.is_empty()
    }

    /// Render the `COM017` diagnostic Platform 16 §16.8 Case C specifies.
    pub fn com017(&self) -> String {
        let mut out = String::from("error[COM017]: cannot instantiate component\n");
        for v in &self.violations {
            out.push_str(&format!("       {v}\n"));
        }
        out.push_str(
            "\nhint: rebuild your component against this host version, or run the host \
             release that provides the required interface version.\n",
        );
        out
    }
}

/// Compare a guest's imports against the interfaces the host provides.
///
/// `host_provided` is what the host actually registers; it is derived from
/// `host.wit` so the check and the published contract cannot drift.
pub fn check_compliance(guest_imports: &[String], host_provided: &[String]) -> ComplianceReport {
    let host: BTreeMap<String, InterfaceRef> = host_provided
        .iter()
        .map(|s| {
            let r = InterfaceRef::parse(s);
            (r.path.clone(), r)
        })
        .collect();

    let mut violations = Vec::new();
    let mut used = BTreeMap::new();

    for imp in guest_imports {
        let want = InterfaceRef::parse(imp);
        match host.get(&want.path) {
            None => violations.push(Violation::MissingInterface { required: want }),
            Some(have) => {
                used.insert(have.path.clone(), ());
                if !have.compatible_with(&want) {
                    violations.push(Violation::VersionMismatch {
                        required: want,
                        provided: have.clone(),
                    });
                }
            }
        }
    }

    let unused_host_interfaces = host
        .into_iter()
        .filter(|(path, _)| !used.contains_key(path))
        .map(|(_, r)| r)
        .collect();

    ComplianceReport {
        violations,
        unused_host_interfaces,
        signature_checked: false,
    }
}

/// Interfaces a guest may import without any bridge or host registration:
/// the standard WASI stack plus `clean:host/*` (CH-03 / CLNH-04).
pub fn is_ambient_interface(path: &str) -> bool {
    path.starts_with("wasi:") || path.starts_with("clean:host/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_interface_and_version_complies() {
        let r = check_compliance(
            &["clean:http/routing@0.1.0".into()],
            &["clean:http/routing@0.1.0".into()],
        );
        assert!(r.is_compliant(), "{:?}", r.violations);
    }

    #[test]
    fn missing_interface_is_refused() {
        // Platform 16 §16.8 Case B: guest wants routing, host has none.
        let r = check_compliance(
            &["clean:http/routing@0.1.0".into()],
            &["clean:host/dom@0.1.0".into()],
        );
        assert!(!r.is_compliant());
        assert!(r.com017().contains("clean:http/routing@0.1.0"));
    }

    #[test]
    fn pre_1_0_minor_bump_is_breaking() {
        // Platform 16 §16.8 Case C: db@0.1.0 vs host db@0.2.0.
        let r = check_compliance(
            &["clean:bridge/db@0.1.0".into()],
            &["clean:bridge/db@0.2.0".into()],
        );
        assert!(!r.is_compliant());
        let msg = r.com017();
        assert!(msg.contains("semver-incompatible"), "{msg}");
        assert!(msg.contains("COM017"), "{msg}");
    }

    #[test]
    fn newer_host_patch_satisfies_an_older_guest() {
        let r = check_compliance(
            &["clean:http/routing@0.1.2".into()],
            &["clean:http/routing@0.1.7".into()],
        );
        assert!(r.is_compliant(), "{:?}", r.violations);
    }

    #[test]
    fn older_host_patch_does_not_satisfy_a_newer_guest() {
        let r = check_compliance(
            &["clean:http/routing@0.1.7".into()],
            &["clean:http/routing@0.1.2".into()],
        );
        assert!(!r.is_compliant());
    }

    #[test]
    fn extra_host_interfaces_are_allowed_but_reported() {
        // HCV-03: compliance is one-way.
        let r = check_compliance(
            &["clean:http/routing@0.1.0".into()],
            &[
                "clean:http/routing@0.1.0".into(),
                "clean:http/websocket@0.1.0".into(),
            ],
        );
        assert!(r.is_compliant());
        assert_eq!(r.unused_host_interfaces.len(), 1);
        assert_eq!(r.unused_host_interfaces[0].path, "clean:http/websocket");
    }

    #[test]
    fn signature_identity_is_not_yet_claimed() {
        let r = check_compliance(&[], &[]);
        assert!(
            !r.signature_checked,
            "presence+version must not be reported as a full HCV-03 pass"
        );
    }

    #[test]
    fn ambient_interfaces_are_recognised() {
        assert!(is_ambient_interface("wasi:cli/environment@0.3.0"));
        assert!(is_ambient_interface("clean:host/log"));
        assert!(!is_ambient_interface("clean:session/store"));
    }
}
