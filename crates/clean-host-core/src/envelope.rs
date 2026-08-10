//! Host-side envelope registration (CLNH-40..43).
//!
//! Some bridges import interfaces the *host* implements — `clean-server`
//! provides `clean:session/http-envelope` and `clean:realtime/sockets` because
//! only the thing holding the HTTP connection can set a cookie or write to a
//! live socket. Concrete hosts register those implementations before `compose`.

use std::collections::BTreeMap;

/// A host-side implementation of an interface bridges may import.
pub trait EnvelopeImpl: Send + Sync {
    /// Interfaces this implementation satisfies, as `package:ns/iface@version`.
    fn interface(&self) -> &str;
}

#[derive(Debug, thiserror::Error)]
pub enum EnvelopeError {
    #[error("envelope `{0}` is already registered")]
    Duplicate(String),
    #[error("bridge `{bridge}` imports envelope `{interface}`, which no host implementation was registered for")]
    Unregistered { bridge: String, interface: String },
}

/// What the concrete host has registered.
#[derive(Default)]
pub struct EnvelopeRegistry {
    entries: BTreeMap<String, Box<dyn EnvelopeImpl>>,
}

impl EnvelopeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// CLNH-43 verifies the implementation's signature against the WIT at
    /// registration time. Signature verification needs the resolved type graph
    /// and lands with the bridge work in Phase 3; today this records the
    /// registration and rejects duplicates.
    pub fn register(
        &mut self,
        interface: &str,
        implementation: Box<dyn EnvelopeImpl>,
    ) -> Result<(), EnvelopeError> {
        if self.entries.contains_key(interface) {
            return Err(EnvelopeError::Duplicate(interface.to_string()));
        }
        self.entries.insert(interface.to_string(), implementation);
        Ok(())
    }

    pub fn contains(&self, interface: &str) -> bool {
        self.entries.contains_key(interface)
    }

    pub fn interfaces(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str);
    impl EnvelopeImpl for Fake {
        fn interface(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn registers_and_reports_interfaces() {
        let mut reg = EnvelopeRegistry::new();
        reg.register(
            "clean:session/http-envelope@0.1.0",
            Box::new(Fake("clean:session/http-envelope@0.1.0")),
        )
        .unwrap();
        assert!(reg.contains("clean:session/http-envelope@0.1.0"));
        assert_eq!(reg.interfaces().len(), 1);
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let mut reg = EnvelopeRegistry::new();
        reg.register("clean:realtime/sockets@0.1.0", Box::new(Fake("x")))
            .unwrap();
        let err = reg
            .register("clean:realtime/sockets@0.1.0", Box::new(Fake("x")))
            .unwrap_err();
        assert!(matches!(err, EnvelopeError::Duplicate(_)));
    }
}
